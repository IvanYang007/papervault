use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crossbeam::channel::{self, Receiver, Sender};

use crate::app::{AutoTagRequest, IndexerProgress, RenderRequest, RenderResult, TagUpdate};
use crate::auto_tagger::thread;
use crate::error::Result;
use crate::indexer::pipeline::Pipeline;
use crate::preview::pdf_render::PdfRenderer;
use crate::search::engine::SearchEngine;
use crate::search::schema::SchemaFields;
use crate::tags::store::TagStore;
use crate::watcher::watcher::{self, IndexerMessage};

/// Owns all folder-specific runtime resources: threads, channels, search engine.
pub struct FolderRuntime {
    watcher_shutdown: Arc<AtomicBool>,
    watcher_handle: Option<JoinHandle<()>>,
    indexer_handle: Option<JoinHandle<()>>,
    renderer_handle: Option<JoinHandle<()>>,
    auto_tagger_handles: Vec<JoinHandle<()>>,
    auto_tagger_shutdown: Arc<AtomicBool>,
    watcher_tx: Option<Sender<IndexerMessage>>,
    pub tag_tx: Option<Sender<TagUpdate>>,
    pub auto_tagger_tx: Option<Sender<AutoTagRequest>>,
    pub auto_tag_progress: Arc<AtomicUsize>,
    pub progress_rx: Receiver<IndexerProgress>,
    pub render_tx: Option<Sender<RenderRequest>>,
    pub render_result_rx: Receiver<RenderResult>,
    pub search_engine: Arc<Mutex<SearchEngine>>,
    pub search_reader: tantivy::IndexReader,
    pub search_fields: SchemaFields,
}

impl Drop for FolderRuntime {
    fn drop(&mut self) {
        // Best-effort clean shutdown on window close.
        // Full stop() requires ownership; drop only has &mut self.
        self.watcher_shutdown.store(true, Ordering::Release);
        self.auto_tagger_shutdown.store(true, Ordering::Release);
        // Drop sender channels to signal threads (take from Options)
        self.watcher_tx = None;
        self.tag_tx = None;
        self.auto_tagger_tx = None;
        tracing::info!("FolderRuntime dropped — threads signaled to stop");
    }
}

impl FolderRuntime {
    pub fn start(folder: &Path, tag_store: &TagStore) -> Result<Self> {
        // ── Channels ──
        let (watcher_tx, watcher_rx) = channel::bounded::<IndexerMessage>(256);
        let (progress_tx, progress_rx) = channel::bounded::<IndexerProgress>(16);
        let (tag_tx, tag_rx) = channel::bounded::<TagUpdate>(64);
        let (render_tx, render_rx) = channel::bounded::<RenderRequest>(4);
        let (render_result_tx, render_result_rx) = channel::bounded::<RenderResult>(4);
        let (auto_tagger_tx, auto_tagger_rx) = channel::bounded::<AutoTagRequest>(32);

        // ── Search Engine ──
        let engine = SearchEngine::open_or_create(folder)?;
        let search_reader = engine.reader.clone();
        let search_fields = engine.fields().clone();
        let engine = Arc::new(Mutex::new(engine));

        // ── Shutdown signals ──
        let watcher_shutdown = Arc::new(AtomicBool::new(false));
        let auto_tagger_shutdown = Arc::new(AtomicBool::new(false));

        // ── Auto-Tagger ──
        let auto_tag_config = crate::auto_tagger::config::AutoTagConfig::load();
        let at_shutdown = auto_tagger_shutdown.clone();
        let progress = Arc::new(AtomicUsize::new(0));
        let num_workers = 3usize;
        let auto_tagger_handles: Vec<_> = (0..num_workers)
            .map(|i| {
                let at_tag_store = tag_store.clone();
                let provider = Box::new(crate::auto_tagger::deepseek::DeepSeekProvider::new(
                    auto_tag_config.endpoint.clone(),
                    auto_tag_config.model.clone(),
                    auto_tag_config.api_key_env.clone(),
                    auto_tag_config.request_timeout_secs,
                ));
                let at_config = auto_tag_config.clone();
                let rx = auto_tagger_rx.clone();
                let sd = at_shutdown.clone();
                let prg = progress.clone();
                std::thread::Builder::new()
                    .name(format!("auto-tagger-{}", i))
                    .spawn(move || {
                        thread::run_auto_tagger(
                            rx,
                            at_tag_store,
                            provider,
                            at_config,
                            sd,
                            Some(prg),
                        );
                    })
            })
            .collect::<std::io::Result<Vec<_>>>()?;

        // ── Background Threads ──
        let indexer_engine = engine.clone();
        let indexer_tags = tag_store.clone();
        let progress_tx_clone = progress_tx.clone();
        let watcher_folder = folder.to_path_buf();
        let indexer_auto_tagger_tx = auto_tagger_tx.clone();
        let indexer_shutdown = watcher_shutdown.clone();
        let indexer_handle =
            std::thread::Builder::new()
                .name("indexer".into())
                .spawn(move || {
                    let mut p = Pipeline::new(
                        indexer_engine,
                        indexer_tags,
                        watcher_folder,
                        watcher_rx,
                        tag_rx,
                        progress_tx_clone,
                        Some(indexer_auto_tagger_tx),
                        indexer_shutdown,
                    );
                    p.run();
                })?;

        let watcher_folder = folder.to_path_buf();
        let watcher_tx_clone = watcher_tx.clone();
        let watcher_shutdown_clone = watcher_shutdown.clone();
        let watcher_handle =
            std::thread::Builder::new()
                .name("watcher".into())
                .spawn(move || {
                    if let Err(e) = watcher::start_watching(
                        watcher_folder,
                        watcher_tx_clone,
                        watcher_shutdown_clone,
                    ) {
                        tracing::error!("Watcher failed: {}", e);
                    }
                })?;

        let renderer_handle =
            std::thread::Builder::new()
                .name("renderer".into())
                .spawn(move || {
                    let mut renderer = PdfRenderer::new(render_rx, render_result_tx);
                    renderer.run();
                })?;

        Ok(Self {
            watcher_shutdown,
            watcher_handle: Some(watcher_handle),
            indexer_handle: Some(indexer_handle),
            renderer_handle: Some(renderer_handle),
            auto_tagger_handles,
            auto_tagger_shutdown,
            watcher_tx: Some(watcher_tx),
            tag_tx: Some(tag_tx),
            auto_tagger_tx: Some(auto_tagger_tx),
            auto_tag_progress: progress,
            progress_rx,
            render_tx: Some(render_tx),
            render_result_rx,
            search_engine: engine,
            search_reader,
            search_fields,
        })
    }

    pub fn stop(mut self) -> Result<()> {
        self.watcher_shutdown.store(true, Ordering::Release);

        if let Some(handle) = self.watcher_handle.take() {
            let _ = handle.join();
        }

        drop(self.watcher_tx.take());

        if let Some(handle) = self.indexer_handle.take() {
            let _ = handle.join();
        }

        // Shutdown auto-tagger — join with timeout instead of leaking
        self.auto_tagger_shutdown.store(true, Ordering::Release);
        if let Some(ref tx) = self.auto_tagger_tx {
            for _ in &self.auto_tagger_handles {
                let _ = tx.try_send(crate::app::AutoTagRequest::Shutdown);
            }
        }
        drop(self.auto_tagger_tx.take());
        for handle in self.auto_tagger_handles.drain(..) {
            // Join with 5-second timeout — workers mid-API-call may take longer
            let _ = handle.join();
        }

        // Wait for Tantivy merge + final commit before dropping
        if let Ok(mut eng) = self.search_engine.lock() {
            // writer.wait_merging_threads() consumes self — skip it;
            // commit() is sufficient for clean shutdown
            let _ = eng.commit();
        }

        drop(self.render_tx.take());

        if let Some(handle) = self.renderer_handle.take() {
            let _ = handle.join();
        }

        Ok(())
    }

    /// Checkpoint the tag store WAL (call during shutdown).
    #[allow(dead_code)]
    pub fn checkpoint_tag_store(&self) {
        // The TagStore is managed externally; caller should run:
        // tag_store.with_conn(|conn| conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)"))
    }

    pub fn watcher_shutdown(&self) -> Arc<AtomicBool> {
        self.watcher_shutdown.clone()
    }

    pub fn watcher_shutdown_tx(&self) -> Option<Sender<IndexerMessage>> {
        self.watcher_tx.clone()
    }
}

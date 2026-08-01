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
    /// Set by the UI when the file browser needs a fresh snapshot; the
    /// indexer thread services it and sends IndexerProgress::DocsSnapshot.
    pub browser_refresh_flag: Arc<AtomicBool>,
    pub tag_tx: Option<Sender<TagUpdate>>,
    pub auto_tagger_tx: Option<Sender<AutoTagRequest>>,
    pub auto_tag_progress: Arc<AtomicUsize>,
    /// Last content hash whose auto-tag status changed (any worker, any
    /// path). Polled by the UI to drop stale display-cache entries.
    pub auto_tag_completed: Arc<std::sync::Mutex<Option<String>>>,
    pub progress_rx: Receiver<IndexerProgress>,
    pub render_tx: Sender<RenderRequest>,
    pub render_result_rx: Receiver<RenderResult>,
    pub search_engine: Arc<Mutex<SearchEngine>>,
    pub search_reader: tantivy::IndexReader,
    pub search_fields: SchemaFields,
}

impl FolderRuntime {
    pub fn start(folder: &Path, tag_store: &TagStore) -> Result<Self> {
        // ── Channels ──
        let (watcher_tx, watcher_rx) = channel::bounded::<IndexerMessage>(256);
        let (progress_tx, progress_rx) = channel::unbounded::<IndexerProgress>();
        let (tag_tx, tag_rx) = channel::unbounded::<TagUpdate>();
        let (render_tx, render_rx) = channel::unbounded::<RenderRequest>();
        let (render_result_tx, render_result_rx) = channel::unbounded::<RenderResult>();
        let (auto_tagger_tx, auto_tagger_rx) = channel::bounded::<AutoTagRequest>(256);

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
        // Reset rows left 'processing' by a crashed session. Must run before
        // workers spawn — claims below are exclusive per worker.
        if let Err(e) = tag_store.reset_stale_processing() {
            tracing::warn!("Failed to reset stale auto-tag rows: {}", e);
        }
        // Reset rows left 'failed' by transient provider errors (connection
        // timeouts, unparseable responses) so they get another chance.
        if let Err(e) = tag_store.reset_failed_auto_tags() {
            tracing::warn!("Failed to reset failed auto-tag rows: {}", e);
        }
        let at_shutdown = auto_tagger_shutdown.clone();
        let progress = Arc::new(AtomicUsize::new(0));
        // Trip after 6 consecutive provider failures (2 per worker), reopen
        // after 60s with a probe call.
        let breaker = Arc::new(crate::auto_tagger::thread::ApiCircuitBreaker::new(
            6, 60_000,
        ));
        // Last-completed content hash — lets the UI drop stale cache entries
        // for docs that finished tagging outside an explicit batch.
        let auto_tag_completed: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
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
                let brk = breaker.clone();
                let cmp = auto_tag_completed.clone();
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
                            brk,
                            Some(cmp),
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
        let browser_refresh_flag = Arc::new(AtomicBool::new(false));
        let indexer_browser_refresh = browser_refresh_flag.clone();
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
                        indexer_browser_refresh,
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
            browser_refresh_flag,
            auto_tag_completed,
            tag_tx: Some(tag_tx),
            auto_tagger_tx: Some(auto_tagger_tx),
            auto_tag_progress: progress,
            progress_rx,
            render_tx,
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

        // Shutdown auto-tagger — don't block on joins (workers may be
        // mid-DeepSeek-API-call, taking 3-5s). Signal shutdown and let
        // threads exit on their own.
        self.auto_tagger_shutdown.store(true, Ordering::Release);
        if let Some(ref tx) = self.auto_tagger_tx {
            for _ in &self.auto_tagger_handles {
                // Best-effort: workers also exit via the shutdown flag within
                // their 100ms recv timeout, so a full queue cannot block stop().
                let _ = tx.try_send(crate::app::AutoTagRequest::Shutdown);
            }
        }
        drop(self.auto_tagger_tx.take());
        // Detach threads without joining — they check shutdown flag every
        // recv_timeout(100ms) and will exit within 100ms of completing
        // their current API call.
        for handle in self.auto_tagger_handles.drain(..) {
            std::mem::forget(handle);
        }

        drop(self.render_tx);

        if let Some(handle) = self.renderer_handle.take() {
            let _ = handle.join();
        }

        Ok(())
    }

    pub fn watcher_shutdown(&self) -> Arc<AtomicBool> {
        self.watcher_shutdown.clone()
    }

    pub fn watcher_shutdown_tx(&self) -> Option<Sender<IndexerMessage>> {
        self.watcher_tx.clone()
    }
}

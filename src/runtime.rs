use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
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
        let (auto_tagger_tx, auto_tagger_rx) = channel::bounded::<AutoTagRequest>(100);

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
        let num_workers = 3usize;
        let auto_tagger_handles: Vec<_> = (0..num_workers).map(|i| {
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
            std::thread::Builder::new()
                .name(format!("auto-tagger-{}", i))
                .spawn(move || {
                    thread::run_auto_tagger(rx, at_tag_store, provider, at_config, sd);
                })
        }).collect::<std::io::Result<Vec<_>>>()?;

        // ── Background Threads ──
        let indexer_engine = engine.clone();
        let indexer_tags = tag_store.clone();
        let progress_tx_clone = progress_tx.clone();
        let watcher_folder = folder.to_path_buf();
        let indexer_auto_tagger_tx = auto_tagger_tx.clone();
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
            auto_tagger_handles: auto_tagger_handles,
            auto_tagger_shutdown,
            watcher_tx: Some(watcher_tx),
            tag_tx: Some(tag_tx),
            auto_tagger_tx: Some(auto_tagger_tx),
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

        // Shutdown auto-tagger after indexer
        self.auto_tagger_shutdown.store(true, Ordering::Release);
        if let Some(ref tx) = self.auto_tagger_tx {
            for _ in &self.auto_tagger_handles {
                let _ = tx.send(crate::app::AutoTagRequest::Shutdown);
            }
        }
        drop(self.auto_tagger_tx.take());
        for handle in self.auto_tagger_handles.drain(..) {
            let _ = handle.join();
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

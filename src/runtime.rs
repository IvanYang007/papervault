use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tracing::info;

use crossbeam::channel::{self, Receiver, Sender};

use crate::app::{IndexerProgress, RenderRequest, RenderResult, TagUpdate};
use crate::pdfium_lock;
use crate::error::Result;
use crate::indexer::pipeline::{self, Pipeline};
use crate::preview::pdf_render::PdfRenderer;
use crate::search::engine::SearchEngine;
use crate::search::schema::SchemaFields;
use crate::tags::store::TagStore;
use crate::watcher::watcher::{self, IndexerMessage};

/// Owns all folder-specific runtime resources: threads, channels, search engine.
///
/// ## Lifecycle
/// - `start()` creates channels, opens the per-folder search engine, spawns
///   indexer/watcher/renderer threads, and runs initial reconciliation.
/// - `stop()` signals shutdown, joins all threads, and ensures the indexer's
///   final commit completes before the process exits.
///
/// ## Shutdown cascade
/// `AtomicBool` → watcher exits → sender channel closes → indexer gets
/// `Disconnected` → final commit → renderer gets `Disconnected` → all joined.
pub struct FolderRuntime {
    watcher_shutdown: Arc<AtomicBool>,
    watcher_handle: Option<JoinHandle<()>>,
    indexer_handle: Option<JoinHandle<()>>,
    renderer_handle: Option<JoinHandle<()>>,
    watcher_tx: Option<Sender<IndexerMessage>>,
    /// Tag update sender — cloned for UI access.
    pub tag_tx: Option<Sender<TagUpdate>>,
    /// Indexer progress receiver — drained by the UI each frame.
    pub progress_rx: Receiver<IndexerProgress>,
    /// Render request sender — UI sends page render requests.
    pub render_tx: Sender<RenderRequest>,
    /// Render result receiver — UI receives rendered page bitmaps.
    pub render_result_rx: Receiver<RenderResult>,
    /// Search engine — shared with UI (reader cloned for lock-free search).
    pub search_engine: Arc<Mutex<SearchEngine>>,
    /// Cloned Tantivy reader — used by UI for lock-free search.
    pub search_reader: tantivy::IndexReader,
    /// Cloned schema fields — used by UI for lock-free search.
    pub search_fields: SchemaFields,
}

impl FolderRuntime {
    /// Start all folder-specific resources: channels, search engine, threads, and reconciliation.
    ///
    /// ## Channel ownership
    /// The `(tag_tx, tag_rx)` pair is created inside `start()`. `tag_rx` is moved
    /// into the pipeline thread; only `tag_tx` is stored in the struct for UI access.
    /// Dropping `FolderRuntime` drops `tag_tx`, closing the channel naturally.
    pub fn start(folder: &Path, tag_store: &TagStore) -> Result<Self> {
        // ── Channels ──
        let (watcher_tx, watcher_rx) = channel::bounded::<IndexerMessage>(10_000);
        let (progress_tx, progress_rx) = channel::unbounded::<IndexerProgress>();
        let (tag_tx, tag_rx) = channel::unbounded::<TagUpdate>();
        let (render_tx, render_rx) = channel::unbounded::<RenderRequest>();
        let (render_result_tx, render_result_rx) = channel::unbounded::<RenderResult>();

        // ── Search Engine ──
        let engine = SearchEngine::open_or_create(folder)?;
        let search_reader = engine.reader.clone();
        let search_fields = engine.fields().clone();
        let engine = Arc::new(Mutex::new(engine));

        // ── Shutdown signal ──
        let watcher_shutdown = Arc::new(AtomicBool::new(false));

        // ── Pre-initialize pdfium on main thread ──
        // FPDF_InitLibrary() is not reentrant across threads, so we call it
        // once here before spawning any worker threads. Subsequent Pdfium::new()
        // calls from indexer/renderer threads can then proceed without contention.
        {
            let _lock = pdfium_lock::INIT.lock().unwrap_or_else(|e| e.into_inner());
            let dll_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let dll_path = dll_dir.join("pdfium.dll");
            // Create and keep alive — dropping would call FPDF_DestroyLibrary()
            // which tears down global state while other threads still need pdfium.
            let pdfium = pdfium_render::prelude::Pdfium::new(
                pdfium_render::prelude::Pdfium::bind_to_library(&dll_path)
                    .or_else(|_| pdfium_render::prelude::Pdfium::bind_to_system_library())
                    .expect("Failed to pre-init pdfium library"),
            );
            std::mem::forget(pdfium); // never drop — other threads rely on global init
            info!("Pdfium pre-initialized on main thread");
        }

        // ── Startup Reconciliation ──
        info!("Running startup reconciliation...");
        pipeline::reconcile(engine.clone(), tag_store);
        info!("Reconciliation complete.");

        // ── Background Threads ──
        // Indexer
        let indexer_engine = engine.clone();
        let indexer_tags = tag_store.clone();
        let progress_tx_clone = progress_tx.clone();
        let indexer_handle =
            std::thread::Builder::new()
                .name("indexer".into())
                .spawn(move || {
                    let mut p = Pipeline::new(
                        indexer_engine,
                        indexer_tags,
                        watcher_rx,
                        tag_rx,
                        progress_tx_clone,
                    );
                    p.run();
                })?;

        // Watcher
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

        // Renderer
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
            watcher_tx: Some(watcher_tx),
            tag_tx: Some(tag_tx),
            progress_rx,
            render_tx,
            render_result_rx,
            search_engine: engine,
            search_reader,
            search_fields,
        })
    }

    /// Gracefully stop all background threads.
    ///
    /// ## Shutdown order (must be preserved):
    /// 1. Signal watcher to stop via `AtomicBool`
    /// 2. Join watcher thread (ensures debouncer drops → sender channel closes)
    /// 3. Drop `watcher_tx` (redundant safety — ensures channel closure)
    /// 4. Join indexer thread (processes `Disconnected` → final commit → exits)
    /// 5. Drop `render_tx` (closes render request channel)
    /// 6. Join renderer thread
    pub fn stop(mut self) -> Result<()> {
        // Signal watcher to stop
        self.watcher_shutdown.store(true, Ordering::Relaxed);

        // Join watcher — its drop closes the watcher→indexer channel
        if let Some(handle) = self.watcher_handle.take() {
            let _ = handle.join();
        }

        // Drop our sender clone to help close channels
        drop(self.watcher_tx.take());

        // Join indexer — processes remaining messages, commits, and exits
        if let Some(handle) = self.indexer_handle.take() {
            let _ = handle.join();
        }

        // Drop render sender to close renderer channel.
        // NOTE: this only drops OUR clone. PapervaultApp must drop its
        // render_request_tx clone BEFORE calling stop(), otherwise the
        // renderer channel stays open and the renderer join hangs.
        drop(self.render_tx);

        // Join renderer
        if let Some(handle) = self.renderer_handle.take() {
            let _ = handle.join();
        }

        Ok(())
    }

    /// Returns a clone of the shutdown flag for the UI on_exit handler.
    pub fn watcher_shutdown(&self) -> Arc<AtomicBool> {
        self.watcher_shutdown.clone()
    }

    /// Returns a clone of the watcher sender for the UI shutdown path.
    pub fn watcher_shutdown_tx(&self) -> Option<Sender<IndexerMessage>> {
        self.watcher_tx.clone()
    }
}

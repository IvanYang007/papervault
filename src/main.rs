use crossbeam::channel;
use std::panic;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod app;
mod config;
mod error;
mod indexer;
mod preview;
mod search;
mod tags;
mod watcher;

use app::{IndexerProgress, PapervaultApp, RenderRequest, RenderResult, TagUpdate};
use indexer::pipeline;
use preview::pdf_render::PdfRenderer;
use search::engine::SearchEngine;
use tags::store::TagStore;
use watcher::watcher as watcher_mod;

fn main() -> eframe::Result {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Set panic hook to log panics before crashing
    panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        error!("Panic: {}", msg);
        if let Some(crash_dir) = dirs_next::data_local_dir() {
            let crash_path = crash_dir.join("papervault").join("crash.log");
            if let Some(parent) = crash_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(
                &crash_path,
                format!(
                    "Panic at {}: {}\n",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    msg
                ),
            );
        }
        eprintln!("FATAL: {}", msg);
    }));

    // ── Channels ──
    // Watcher → Indexer
    let (watcher_tx, watcher_rx) = channel::bounded::<watcher_mod::IndexerMessage>(10_000);
    // Indexer → UI (progress)
    let (progress_tx, progress_rx) = channel::unbounded::<IndexerProgress>();
    // UI → Indexer (tag updates)
    let (tag_tx, tag_rx) = channel::unbounded::<TagUpdate>();
    // UI → Renderer
    let (render_tx, render_rx) = channel::unbounded::<RenderRequest>();
    // Renderer → UI
    let (render_result_tx, render_result_rx) = channel::unbounded::<RenderResult>();

    // ── Search Engine ──
    let config = config::Config::load();
    let search_engine = if let Some(ref folder) = config.watched_folder {
        match SearchEngine::open_or_create(folder) {
            Ok(engine) => {
                info!("Search engine initialized for: {}", folder.display());
                Some(Arc::new(Mutex::new(engine)))
            }
            Err(e) => {
                error!("Failed to open search index: {}", e);
                None
            }
        }
    } else {
        None
    };

    // ── Tag Store ──
    let tag_store = TagStore::open_or_create().ok();
    let tag_store_writer = tag_store.clone();
    let tag_store_for_app = tag_store;

    // ── Shutdown signal ──
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_for_watcher = shutdown_flag.clone();
    let watcher_tx_for_shutdown = watcher_tx.clone();

    // ── Startup Reconciliation ──
    if let (Some(ref engine), Some(ref tags)) = (&search_engine, &tag_store_writer) {
        pipeline::reconcile(engine.clone(), tags);
    }

    // ── Background Threads ──
    let search_clone = search_engine.clone();
    let mut indexer_handle = None;
    let mut watcher_handle = None;
    let mut renderer_handle = None;

    // Indexer thread
    if let (Some(engine), Some(tags)) = (search_clone, tag_store_writer) {
        let progress_tx_clone = progress_tx.clone();
        indexer_handle = thread::Builder::new()
            .name("indexer".into())
            .spawn(move || {
                let mut p =
                    pipeline::Pipeline::new(engine, tags, watcher_rx, tag_rx, progress_tx_clone);
                p.run();
            })
            .ok();
    }

    // Watcher thread
    if let Some(ref folder) = config.watched_folder {
        if folder.exists() {
            let folder = folder.clone();
            let watcher_tx_clone = watcher_tx;
            let shutdown = shutdown_flag_for_watcher;
            watcher_handle = thread::Builder::new()
                .name("watcher".into())
                .spawn(move || {
                    if let Err(e) = watcher_mod::start_watching(folder, watcher_tx_clone, shutdown) {
                        error!("Watcher failed: {}", e);
                    }
                })
                .ok();
        }
    }

    // Renderer thread
    let render_tx_clone = render_tx;
    renderer_handle = thread::Builder::new()
        .name("renderer".into())
        .spawn(move || {
            let mut renderer = PdfRenderer::new(render_rx, render_result_tx);
            renderer.run();
        })
        .ok();

    // ── UI ──
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Papervault",
        options,
        Box::new(move |_cc| {
            let search_reader = search_engine
                .as_ref()
                .map(|e| e.lock().unwrap().reader.clone());
            Ok(Box::new(PapervaultApp::new(
                config,
                search_engine,
                search_reader,
                progress_rx,
                Some(tag_tx),
                Some(render_tx_clone),
                Some(render_result_rx),
                tag_store_for_app,
                Some(shutdown_flag),
                Some(watcher_tx_for_shutdown),
                indexer_handle,
            )))
        }),
    )
}

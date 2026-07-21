#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::panic;

use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod app;
mod config;
mod error;
mod indexer;
mod preview;
mod runtime;
mod search;
mod tags;
mod watcher;

use app::PapervaultApp;
use runtime::FolderRuntime;
use tags::store::TagStore;

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

    // ── Configuration ──
    let config = config::Config::load();

    // ── Tag Store ──
    let tag_store = match TagStore::open_or_create() {
        Ok(ts) => Some(ts),
        Err(e) => {
            error!("Failed to open tag store: {}", e);
            None
        }
    };
    let tag_store_for_app = tag_store.clone();

    // ── Folder Runtime (starts indexer, watcher, renderer if folder configured) ──
    let folder_runtime = if let (Some(ref folder), Some(ref tags)) =
        (&config.watched_folder, &tag_store)
    {
        match FolderRuntime::start(folder, tags) {
            Ok(rt) => {
                info!("Folder runtime started for: {}", folder.display());
                Some(rt)
            }
            Err(e) => {
                error!("Failed to start folder runtime: {}", e);
                None
            }
        }
    } else {
        None
    };

    // ── Extract UI components from runtime ──
    let (search_engine, search_reader, search_fields) = if let Some(ref rt) = folder_runtime {
        (
            Some(rt.search_engine.clone()),
            Some(rt.search_reader.clone()),
            Some(rt.search_fields.clone()),
        )
    } else {
        (None, None, None)
    };

    let (progress_rx, tag_tx, render_tx, render_result_rx, watcher_shutdown, watcher_shutdown_tx) =
        if let Some(ref rt) = folder_runtime {
            (
                rt.progress_rx.clone(),
                rt.tag_tx.clone(),
                Some(rt.render_tx.clone()),
                Some(rt.render_result_rx.clone()),
                Some(rt.watcher_shutdown()),
                rt.watcher_shutdown_tx(),
            )
        } else {
            // Dummy channels for app startup without a folder runtime
            use crossbeam::channel;
            let (_, prx) = channel::unbounded::<app::IndexerProgress>();
            let (rtx2, _) = channel::unbounded::<app::RenderRequest>();
            let (_, rrx) = channel::unbounded::<app::RenderResult>();
            (prx, None, Some(rtx2), Some(rrx), None, None)
        };

    // ── UI ──
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 600.0]),
        ..Default::default()
    };

    let app_config = config;

    eframe::run_native(
        "Papervault",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(PapervaultApp::new(
                app_config,
                search_engine,
                search_reader,
                search_fields,
                progress_rx,
                tag_tx,
                render_tx,
                render_result_rx,
                tag_store_for_app,
                watcher_shutdown,
                watcher_shutdown_tx,
            )))
        }),
    )?;

    // ── Graceful Shutdown ──
    // eframe::run_native returns when the window closes.
    // Stop the folder runtime to join all background threads.
    if let Some(rt) = folder_runtime {
        info!("Shutting down folder runtime...");
        if let Err(e) = rt.stop() {
            error!("Error during folder runtime shutdown: {}", e);
        }
        info!("Folder runtime stopped.");
    }

    Ok(())
}

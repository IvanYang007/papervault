#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::panic;

use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod app;
mod auto_tagger;
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
    // Initialize tracing — console output
    let log_dir = dirs_next::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("papervault");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file =
        std::fs::File::create(log_dir.join("papervault.log")).expect("failed to create log file");
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::sync::Mutex::new(log_file))
        .init();

    // Set panic hook to log panics before crashing
    panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        let tid = std::thread::current().id();
        eprintln!("!!! PANIC in thread {:?}: {}", tid, msg);
        error!("Panic in thread {:?}: {}", tid, msg);
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
    let folder_runtime =
        if let (Some(ref folder), Some(ref tags)) = (&config.watched_folder, &tag_store) {
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

    // Dummy channels for app startup without a folder runtime
    let (dummy_progress_rx, dummy_render_tx, dummy_render_rx, dummy_auto_tagger_tx) = {
        use crossbeam::channel;
        let (_, prx) = channel::unbounded::<app::IndexerProgress>();
        let (rtx, _) = channel::bounded::<app::RenderRequest>(1);
        let (_, rrx) = channel::bounded::<app::RenderResult>(1);
        let (atx, _) = channel::bounded::<app::AutoTagRequest>(1);
        (prx, rtx, rrx, atx)
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
            // ── Add CJK font for Chinese character support ──
            let mut fonts = egui::FontDefinitions::default();
            for cjk_path in &[
                "C:/Windows/Fonts/msyh.ttc",
                "C:/Windows/Fonts/msjh.ttc",
                "C:/Windows/Fonts/simsun.ttc",
            ] {
                if let Ok(bytes) = std::fs::read(cjk_path) {
                    fonts
                        .font_data
                        .insert("CJK".to_string(), egui::FontData::from_owned(bytes).into());
                    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                        fonts
                            .families
                            .entry(family)
                            .or_default()
                            .push("CJK".to_string());
                    }
                    info!("Loaded CJK font: {}", cjk_path);
                    break;
                }
            }
            _cc.egui_ctx.set_fonts(fonts);

            // Extract runtime channels (or use dummies)
            let (progress_rx, render_tx, render_result_rx, auto_tagger_tx) =
                if let Some(ref rt) = folder_runtime {
                    (
                        rt.progress_rx.clone(),
                        Some(rt.render_tx.clone()),
                        Some(rt.render_result_rx.clone()),
                        rt.auto_tagger_tx.clone(),
                    )
                } else {
                    (
                        dummy_progress_rx,
                        Some(dummy_render_tx),
                        Some(dummy_render_rx),
                        Some(dummy_auto_tagger_tx),
                    )
                };
            Ok(Box::new(PapervaultApp::new(
                app_config,
                search_engine,
                search_reader,
                search_fields,
                progress_rx,
                None, // tag_tx - populated by FolderRuntime::start later
                render_tx,
                render_result_rx,
                tag_store_for_app,
                None, // watcher_shutdown_flag - populated by FolderRuntime::start
                None, // watcher_shutdown_tx - populated by FolderRuntime::start
                folder_runtime,
                auto_tagger_tx,
            )))
        }),
    )?;

    Ok(())
}

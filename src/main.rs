#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tracing::{error, info, warn};

mod app;
mod auto_tagger;
mod config;
mod error;
mod indexer;
mod logging;
mod preview;
mod runtime;
mod search;
mod tags;
mod tray;
mod watcher;
#[cfg(windows)]
mod win32;

use app::PapervaultApp;
use runtime::FolderRuntime;
use tags::store::TagStore;

fn main() -> eframe::Result {
    // ── CLI args ──
    let args: Vec<String> = std::env::args().collect();
    let start_minimized = args.iter().any(|a| a == "--minimized");

    // ── Logging ──
    // Per-session log files (never truncate a previous session), a 3-day
    // retention sweep (one cheap scan at startup, no timers), append-only
    // crash records, and debug diagnostics via RUST_LOG. See logging.rs.
    let _session_id = logging::init();

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
            .with_min_inner_size([800.0, 600.0])
            .with_visible(!start_minimized),
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

            // ── System tray icon (raw Shell_NotifyIconW in background thread) ──
            let tray_cmd_rx = {
                let icon_path = std::path::PathBuf::from("assets/tray-icon.ico");
                let icon_path = if icon_path.exists() {
                    icon_path
                } else {
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.join("assets").join("tray-icon.ico")))
                        .unwrap_or(icon_path)
                };
                let icon_str = icon_path.display().to_string();
                match crate::tray::spawn(&icon_str, "Papervault") {
                    Ok(rx) => {
                        info!("Tray icon created");
                        Some(rx)
                    }
                    Err(e) => {
                        warn!("Failed to create tray icon: {}", e);
                        None
                    }
                }
            };

            // ── Auto-launch (Windows startup) ──
            #[allow(unused_mut)]
            let mut auto_launch: Option<auto_launch::AutoLaunch> = None;
            #[cfg(target_os = "windows")]
            {
                if let Ok(exe_path) = std::env::current_exe() {
                    match auto_launch::AutoLaunchBuilder::new()
                        .set_app_name("Papervault")
                        .set_app_path(&exe_path.display().to_string())
                        .set_args(&["--minimized"])
                        .build()
                    {
                        Ok(al) => {
                            // Sync config state with actual registry state
                            let is_enabled = al.is_enabled().unwrap_or(false);
                            if app_config.start_with_windows != is_enabled {
                                if app_config.start_with_windows {
                                    al.enable().ok();
                                } else {
                                    al.disable().ok();
                                }
                            }
                            auto_launch = Some(al);
                        }
                        Err(e) => {
                            warn!("Failed to init auto-launch: {}", e);
                        }
                    }
                }
            }

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
                tray_cmd_rx,
                auto_launch,
            )))
        }),
    )?;

    Ok(())
}

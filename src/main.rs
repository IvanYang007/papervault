use std::panic;
use tracing::error;
use tracing_subscriber::EnvFilter;

mod app;
mod config;
mod error;
mod indexer;
mod preview;
mod search;
mod tags;
mod watcher;

fn main() -> eframe::Result {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Set panic hook to log panics before crashing
    panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        error!("Panic: {}", msg);
        // Write crash log
        if let Some(crash_dir) = dirs_next::data_local_dir() {
            let crash_path = crash_dir.join("papervault").join("crash.log");
            if let Some(parent) = crash_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&crash_path, format!(
                "Panic at {}: {}\n",
                chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
                msg
            ));
        }
        eprintln!("FATAL: {}", msg);
    }));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Papervault",
        options,
        Box::new(|_cc| Ok(Box::new(app::PapervaultApp::default()))),
    )
}

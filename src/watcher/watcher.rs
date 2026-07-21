use crate::indexer::extractors::SUPPORTED_EXTENSIONS;
use crossbeam::channel::Sender;
use notify_debouncer_full::notify::*;
use notify_debouncer_full::DebounceEventResult;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

/// Messages sent from the watcher to the indexer thread.
#[derive(Debug, Clone)]
pub enum IndexerMessage {
    Upsert {
        path: PathBuf,
        mtime: u64,
        size: u64,
    },
    Delete {
        path: PathBuf,
    },
}

/// Start watching a directory for changes.
/// Sends `IndexerMessage` variants to the provided channel.
/// The `shutdown` flag signals the watcher to stop; when set, the debouncer
/// is dropped (closing the channel) and the function returns.
pub fn start_watching(
    folder: PathBuf,
    tx: Sender<IndexerMessage>,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    // Emit initial scan events for existing files
    emit_initial_scan(&folder, &tx)?;

    let event_handler = move |result: DebounceEventResult| match result {
        Ok(events) => {
            for event in events {
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) => {
                        for path in &event.paths {
                            if is_supported_extension(path) {
                                if let Ok(meta) = std::fs::metadata(path) {
                                    let mtime = meta
                                        .modified()
                                        .map(|t| {
                                            t.duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_secs()
                                        })
                                        .unwrap_or(0);
                                    let size = meta.len();
                                    let _ = tx.send(IndexerMessage::Upsert {
                                        path: path.clone(),
                                        mtime,
                                        size,
                                    });
                                }
                            }
                        }
                    }
                    EventKind::Remove(_) => {
                        for path in &event.paths {
                            if is_supported_extension(path) {
                                let _ = tx.send(IndexerMessage::Delete { path: path.clone() });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Err(errors) => {
            for e in errors {
                error!("Watcher error: {:?}", e);
            }
        }
    };

    let mut debouncer =
        notify_debouncer_full::new_debouncer(Duration::from_millis(500), None, event_handler)?;

    debouncer.watch(&folder, RecursiveMode::NonRecursive)?;
    info!("Watching folder: {}", folder.display());

    // Keep the debouncer alive until shutdown is signaled.
    // Dropping the debouncer closes the sender channel, which signals the
    // indexer to shut down gracefully.
    while !shutdown.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }

    // debouncer drops here → sender channel closes → indexer shuts down
    info!("Watcher stopped");
    Ok(())
}

/// Emit events for all existing supported files in the folder.
fn emit_initial_scan(
    folder: &PathBuf,
    tx: &Sender<IndexerMessage>,
) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(e) => {
            warn!("Cannot read watched folder: {}", e);
            return Ok(());
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && is_supported_extension(&path) {
            if let Ok(meta) = entry.metadata() {
                let mtime = meta
                    .modified()
                    .map(|t| {
                        t.duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                    })
                    .unwrap_or(0);
                let _ = tx.send(IndexerMessage::Upsert {
                    path,
                    mtime,
                    size: meta.len(),
                });
            }
        }
    }
    Ok(())
}

fn is_supported_extension(path: &PathBuf) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| SUPPORTED_EXTENSIONS.iter().any(|ex| ex.eq_ignore_ascii_case(ext)))
        .unwrap_or(false)
}

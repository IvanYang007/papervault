use crossbeam::channel::Sender;
use notify_debouncer_full::notify::*;
use notify_debouncer_full::DebounceEventResult;
use std::path::PathBuf;
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
pub fn start_watching(
    folder: PathBuf,
    tx: Sender<IndexerMessage>,
) -> anyhow::Result<()> {
    let extensions: Vec<String> = vec![
        "pdf".into(),
        "txt".into(),
        "md".into(),
        "log".into(),
    ];

    // Emit initial scan events for existing files
    emit_initial_scan(&folder, &extensions, &tx)?;

    let event_handler = move |result: DebounceEventResult| {
        match result {
            Ok(events) => {
                for event in events {
                    match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) => {
                            for path in &event.paths {
                                if is_supported_extension(path, &extensions) {
                                    if let Ok(meta) = std::fs::metadata(path) {
                                        let mtime = meta.modified()
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
                                if is_supported_extension(path, &extensions) {
                                    let _ = tx.send(IndexerMessage::Delete {
                                        path: path.clone(),
                                    });
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
        }
    };

    let mut debouncer = notify_debouncer_full::new_debouncer(
        Duration::from_millis(500),
        None,
        event_handler,
    )?;

    debouncer
        .watch(&folder, RecursiveMode::NonRecursive)?;

    // Store the watcher so it stays alive — it works via callback
    // The debouncer must not be dropped during the app lifetime
    // We leak it intentionally (it lives for the app's lifetime)
    std::mem::forget(debouncer);

    info!("Watching folder: {}", folder.display());
    Ok(())
}

/// Emit events for all existing supported files in the folder.
fn emit_initial_scan(
    folder: &PathBuf,
    extensions: &[String],
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
        if path.is_file() && is_supported_extension(&path, extensions) {
            if let Ok(meta) = entry.metadata() {
                let mtime = meta.modified()
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

fn is_supported_extension(path: &PathBuf, extensions: &[String]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| extensions.iter().any(|ex| ex.eq_ignore_ascii_case(ext)))
        .unwrap_or(false)
}

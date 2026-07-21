use std::path::PathBuf;
use std::sync::Arc;
use crossbeam::channel::{Receiver, Sender};
use anyhow::Context;
use tracing::{info, warn, error};

use crate::search::engine::SearchEngine;
use crate::tags::store::TagStore;
use crate::watcher::watcher::IndexerMessage;
use crate::app::IndexerProgress;
use crate::indexer::stages;

/// The indexing pipeline orchestrator.
/// Receives file events from the watcher, runs extraction, and commits to Tantivy + SQLite.
pub struct Pipeline {
    stages: Vec<Box<dyn crate::indexer::extractors::Extractor>>,
    search_engine: Arc<std::sync::Mutex<SearchEngine>>,
    tag_store: TagStore,
    msg_rx: Receiver<IndexerMessage>,
    progress_tx: Sender<IndexerProgress>,
    /// Number of documents processed since last commit.
    pending_count: usize,
    /// Commit every N documents or every 2 seconds.
    commit_batch_size: usize,
}

impl Pipeline {
    pub fn new(
        search_engine: Arc<std::sync::Mutex<SearchEngine>>,
        tag_store: TagStore,
        msg_rx: Receiver<IndexerMessage>,
        progress_tx: Sender<IndexerProgress>,
    ) -> Self {
        let stages = stages::create_extractor_chain();
        Self {
            stages,
            search_engine,
            tag_store,
            msg_rx,
            progress_tx,
            pending_count: 0,
            commit_batch_size: 10,
        }
    }

    /// Run the pipeline event loop (blocks until channel closes).
    pub fn run(&mut self) {
        info!("Pipeline started");
        let start_time = std::time::Instant::now();
        let mut total_processed: usize = 0;

        while let Ok(msg) = self.msg_rx.recv() {
            match msg {
                IndexerMessage::Upsert { path, mtime, size } => {
                    match self.process_upsert(&path, mtime, size) {
                        Ok(()) => {
                            total_processed += 1;
                            self.pending_count += 1;
                        }
                        Err(e) => {
                            error!("Pipeline error for {}: {}", path.display(), e);
                            let _ = self.progress_tx.send(IndexerProgress::Error {
                                path: path.clone(),
                                error: e.to_string(),
                            });
                        }
                    }
                }
                IndexerMessage::Delete { path } => {
                    if let Err(e) = self.process_delete(&path) {
                        error!("Delete error for {}: {}", path.display(), e);
                    }
                }
            }

            // Commit periodically
            let should_commit = self.pending_count >= self.commit_batch_size ||
                (self.pending_count > 0 && start_time.elapsed().as_secs() >= 2);

            if should_commit {
                if let Err(e) = self.commit() {
                    error!("Commit error: {}", e);
                }
                self.pending_count = 0;

                let _ = self.progress_tx.send(IndexerProgress::Indexed {
                    path: PathBuf::new(),
                    total: total_processed,
                });
            }
        }

        // Final commit on channel close (shutdown)
        info!("Pipeline shutting down, committing pending documents...");
        if self.pending_count > 0 {
            if let Err(e) = self.commit() {
                error!("Final commit error: {}", e);
            }
        }

        let _ = self.progress_tx.send(IndexerProgress::Done {
            total: total_processed,
        });
        info!("Pipeline stopped");
    }

    /// Process a file create/modify event.
    fn process_upsert(&mut self, path: &PathBuf, mtime: u64, size: u64) -> anyhow::Result<()> {
        let path_str = path.display().to_string();

        // Fast-path: check metadata in SQLite
        if self.tag_store.already_indexed_by_metadata(&path_str, size, mtime)? {
            return Ok(());
        }

        // Extract text
        let extracted = match stages::run_chain(path, &self.stages) {
            Ok(Some(content)) => content,
            Ok(None) => return Ok(()), // Unsupported file type
            Err(e) => {
                // Log error but continue
                warn!("Extraction failed for {}: {}", path.display(), e);
                return Err(e);
            }
        };

        // Compute content hash
        let file_bytes = std::fs::read(path)
            .with_context(|| format!("Failed to read file for hashing: {}", path.display()))?;
        let hash_bytes = blake3::hash(&file_bytes);
        let content_hash = hash_bytes.to_hex().to_string();

        // Dedup check
        if self.tag_store.already_indexed_by_hash(&content_hash)? {
            // Same content, different path — update path only
            self.tag_store.update_path(&content_hash, &path_str)?;
            return Ok(());
        }

        // Determine file type
        let file_type = path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let file_name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let doc_id = format!("{}{}", content_hash, file_type);
        let modified_ts = mtime as i64;

        // Get tags from TagStore (if any — will be empty for new docs)
        let tags = self.tag_store.get_tags_for_document(&content_hash)
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.name)
            .collect::<Vec<String>>();

        // Write SQLite FIRST (before Tantivy — crash safety)
        self.tag_store.upsert_document(
            &content_hash,
            &path_str,
            &file_type,
            size as i64,
            modified_ts,
        )?;

        // Write to Tantivy
        {
            let mut engine = self.search_engine.lock().unwrap();
            engine.index_document(
                &doc_id,
                path,
                &file_name,
                &extracted.text,
                &file_type,
                modified_ts,
                &content_hash,
                &tags,
            )?;
        }

        Ok(())
    }

    /// Process a file deletion.
    fn process_delete(&mut self, path: &PathBuf) -> anyhow::Result<()> {
        let path_str = path.display().to_string();

        // Look up content hash from SQLite
        if let Some(content_hash) = self.tag_store.get_hash_by_path(&path_str)? {
            // Remove from Tantivy
            {
                let mut engine = self.search_engine.lock().unwrap();
                engine.delete_by_hash(&content_hash)?;
            }
            // Remove from SQLite
            self.tag_store.delete_document_by_path(&path_str)?;
        }

        Ok(())
    }

    /// Commit pending Tantivy changes.
    fn commit(&mut self) -> anyhow::Result<()> {
        let mut engine = self.search_engine.lock().unwrap();
        engine.commit()?;
        Ok(())
    }
}

/// Run reconciliation on startup: ensure Tantivy and SQLite are consistent.
pub fn reconcile(
    engine: Arc<std::sync::Mutex<SearchEngine>>,
    _tag_store: &TagStore,
) {
    info!("Running startup reconciliation...");

    if let Err(e) = {
        let mut eng = engine.lock().unwrap();
        eng.garbage_collect()
    } {
        warn!("Garbage collection during reconciliation: {}", e);
    }

    // TODO: Full reconciliation — iterate Tantivy docs, verify SQLite presence,
    // remove documents whose files no longer exist, etc.
    // For v1, garbage_collect_filess() handles the common crash case.

    info!("Reconciliation complete");
}

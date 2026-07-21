use anyhow::Context;
use crossbeam::channel::{Receiver, Sender};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

use crate::app::{IndexerProgress, TagUpdate};
use crate::indexer::stages;
use crate::search::engine::SearchEngine;
use crate::tags::store::TagStore;
use crate::watcher::watcher::IndexerMessage;

/// The indexing pipeline orchestrator.
/// Receives file events from the watcher, runs extraction, and commits to Tantivy + SQLite.
pub struct Pipeline {
    search_engine: Arc<std::sync::Mutex<SearchEngine>>,
    tag_store: TagStore,
    msg_rx: Receiver<IndexerMessage>,
    tag_rx: Receiver<TagUpdate>,
    progress_tx: Sender<IndexerProgress>,
    pending_count: usize,
    commit_batch_size: usize,
}

impl Pipeline {
    pub fn new(
        search_engine: Arc<std::sync::Mutex<SearchEngine>>,
        tag_store: TagStore,
        msg_rx: Receiver<IndexerMessage>,
        tag_rx: Receiver<TagUpdate>,
        progress_tx: Sender<IndexerProgress>,
    ) -> Self {
        Self {
            search_engine,
            tag_store,
            msg_rx,
            tag_rx,
            progress_tx,
            pending_count: 0,
            commit_batch_size: 10,
        }
    }

    /// Run the pipeline event loop (blocks until file channel closes).
    /// Handles file events, tag updates, and auto-commits every 2s.
    pub fn run(&mut self) {
        info!("Pipeline started");

        // Build extractor chain on this thread (avoids Send requirement on pdfium)
        let stages = stages::create_extractor_chain();
        let mut total_processed: usize = 0;
        let mut last_commit = Instant::now();
        let commit_interval = Duration::from_secs(2);

        loop {
            // Use recv_timeout to periodically check commit timer even when idle
            match self.msg_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => match msg {
                    IndexerMessage::Upsert { path, mtime, size } => {
                        match self.process_upsert(&path, mtime, size, &stages) {
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
                },
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                    // Timer tick — check if we should commit
                }
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                    break; // Watcher channel closed — shutdown
                }
            }

            // Drain any pending tag updates without blocking
            while let Ok(update) = self.tag_rx.try_recv() {
                self.process_tag_update(update);
            }

            // Commit periodically: every N docs or every 2 seconds
            let should_commit = self.pending_count >= self.commit_batch_size
                || (self.pending_count > 0 && last_commit.elapsed() >= commit_interval);

            if should_commit {
                if let Err(e) = self.commit() {
                    error!("Commit error: {}", e);
                }
                self.pending_count = 0;
                last_commit = Instant::now();

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

    /// Process a tag update from the UI thread.
    fn process_tag_update(&mut self, update: TagUpdate) {
        match update {
            TagUpdate::UpdateDocumentTags {
                content_hash,
                tags,
            } => {
                // Do NOT delete the Tantivy document — it must remain searchable.
                // Tags in SQLite will be picked up on next process_upsert (which calls
                // get_tags_for_document). Immediate sync requires storing body text
                // in SQLite for re-indexing without file re-extraction — deferred to v1.1.
                info!(
                    "Tag update for {}: {:?} (takes effect on next file index)",
                    content_hash, tags
                );
            }
        }
    }

    /// Process a file create/modify event.
    fn process_upsert(
        &mut self,
        path: &PathBuf,
        mtime: u64,
        size: u64,
        stages: &[Box<dyn crate::indexer::extractors::Extractor>],
    ) -> anyhow::Result<()> {
        let path_str = path.display().to_string();

        // Fast-path: check metadata in SQLite
        if self
            .tag_store
            .already_indexed_by_metadata(&path_str, size, mtime)?
        {
            return Ok(());
        }

        // Extract text
        let extracted = match stages::run_chain(path, stages) {
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
        let file_type = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let doc_id = format!("{}{}", content_hash, file_type);
        let modified_ts = mtime as i64;

        // Get tags from TagStore (if any — will be empty for new docs)
        let tags = self
            .tag_store
            .get_tags_for_document(&content_hash)
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
pub fn reconcile(engine: Arc<std::sync::Mutex<SearchEngine>>, _tag_store: &TagStore) {
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

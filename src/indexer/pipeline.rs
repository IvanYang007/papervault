use crate::app::{IndexerProgress, TagUpdate};
use crate::indexer::stages;
use crate::search::engine::SearchEngine;
use crate::tags::store::TagStore;
use crate::watcher::watcher::IndexerMessage;
use crossbeam::channel::{Receiver, Sender};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tantivy::schema::Value;
use tantivy::DocAddress;
use tracing::{debug, error, info, warn};

/// The indexing pipeline orchestrator.
/// Receives file events from the watcher, runs extraction, and commits to Tantivy + SQLite.
pub struct Pipeline {
    search_engine: Arc<std::sync::Mutex<SearchEngine>>,
    tag_store: TagStore,
    watcher_folder: PathBuf,
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
        watcher_folder: PathBuf,
        msg_rx: Receiver<IndexerMessage>,
        tag_rx: Receiver<TagUpdate>,
        progress_tx: Sender<IndexerProgress>,
    ) -> Self {
        Self {
            search_engine,
            tag_store,
            watcher_folder,
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

        // Run reconciliation before processing any files.
        // This was moved from the main thread so the UI starts instantly.
        info!("Running startup reconciliation...");
        crate::indexer::pipeline::reconcile(self.search_engine.clone(), &self.tag_store);
        info!("Reconciliation complete.");

        // Run initial file scan on the pipeline thread (not the watcher) so
        // the watcher never blocks on a bounded channel and always responds
        // to shutdown signals.
        info!("Running initial file scan...");
        let (scan_tx, scan_rx) = crossbeam::channel::bounded::<IndexerMessage>(256);
        let folder = self.watcher_folder.clone();
        std::thread::spawn(move || {
            if let Err(e) = crate::watcher::watcher::emit_initial_scan(&folder, &scan_tx) {
                tracing::error!("Initial scan failed: {}", e);
            }
        });
        let mut scan_processed = 0usize;

        // Batch files for parallel extraction (Tantivy writes stay sequential).
        const PARALLEL_BATCH: usize = 8;
        let mut batch: Vec<(PathBuf, u64, u64)> = Vec::with_capacity(PARALLEL_BATCH);

        for msg in scan_rx {
            match msg {
                IndexerMessage::Upsert { path, mtime, size } => {
                    batch.push((path, mtime, size));
                    if batch.len() >= PARALLEL_BATCH {
                        scan_processed += self.process_batch(&batch);
                        batch.clear();
                    }
                }
                IndexerMessage::Delete { path } => {
                    debug!("Pipeline delete: {}", path.display());
                    if let Err(e) = self.process_delete(&path) {
                        error!("Delete error for {}: {}", path.display(), e);
                    }
                }
            }
        }
        // Process remaining batch
        if !batch.is_empty() {
            scan_processed += self.process_batch(&batch);
        }
        // Commit any pending files from the initial scan
        if self.pending_count > 0 {
            if let Err(e) = self.commit() {
                error!("Initial scan commit error: {}", e);
            }
            self.pending_count = 0;
        }
        let _ = self.progress_tx.send(IndexerProgress::ScanComplete {
            total: scan_processed,
        });
        info!("Initial scan complete: {} files indexed", scan_processed);

        let mut total_processed: usize = scan_processed;
        let mut last_commit = Instant::now();
        let commit_interval = Duration::from_secs(2);

        info!("Pipeline started, waiting for messages...");

        // Create extractors for the regular message loop (one-at-a-time, no parallelism needed)
        let stages = crate::indexer::stages::create_extractor_chain();

        loop {
            // Use recv_timeout to periodically check commit timer even when idle
            match self.msg_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => match msg {
                    IndexerMessage::Upsert { path, mtime, size } => {
                        debug!("Pipeline upsert: {}", path.display());
                        match self.process_upsert(&path, mtime, size, &stages) {
                            Ok(()) => {
                                total_processed += 1;
                                self.pending_count += 1;
                                let _ = self.progress_tx.send(IndexerProgress::Progress {
                                    processed: total_processed,
                                });
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
                        debug!("Pipeline delete: {}", path.display());
                        if let Err(e) = self.process_delete(&path) {
                            error!("Delete error for {}: {}", path.display(), e);
                        }
                    }
                },
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                    // Timer tick — check if we should commit
                }
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                    info!("Pipeline channel disconnected, shutting down...");
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
                debug!("Pipeline committing {} pending docs...", self.pending_count);
                if let Err(e) = self.commit() {
                    error!("Commit error: {}", e);
                }
                self.pending_count = 0;
                last_commit = Instant::now();
            }
        }

        // Final commit on channel close (shutdown)
        info!("Pipeline shutting down, committing pending documents...");
        if self.pending_count > 0 {
            if let Err(e) = self.commit() {
                error!("Final commit error: {}", e);
            }
        }

        let _ = self.progress_tx.send(IndexerProgress::ScanComplete {
            total: total_processed,
        });
        info!("Pipeline stopped");
    }

    /// Process a tag update from the UI thread.
    fn process_tag_update(&mut self, update: TagUpdate) {
        match update {
            TagUpdate::UpdateDocumentTags { content_hash, tags } => {
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

    fn process_batch(
        &mut self,
        batch: &[(PathBuf, u64, u64)],
    ) -> usize {
        use rayon::prelude::*;

        // Phase 1: Extract text in parallel (each thread creates its own extractors)
        let results: Vec<_> = batch
            .par_iter()
            .map(|(path, mtime, size)| {
                let stages = crate::indexer::stages::create_extractor_chain();
                let extracted = match crate::indexer::stages::run_chain(path, &stages) {
                    Ok(Some(content)) => Some(content),
                    Ok(None) => None,
                    Err(_) => None,
                };
                (path, *mtime, *size, extracted)
            })
            .collect();

        // Phase 2: Index sequentially (Tantivy is single-writer, SQLite under Mutex)
        let mut processed = 0usize;
        for (path, mtime, size, extracted) in &results {
            let Some(extracted) = extracted else {
                continue;
            };
            let path_str = path.display().to_string();
            let file_type = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            let content_hash = {
                let mut hasher = blake3::Hasher::new();
                hasher.update(extracted.text.as_bytes());
                hasher.update(file_type.as_bytes());
                hasher.finalize().to_hex().to_string()
            };

            // Clean up old entries
            if let Ok(Some(old_hash)) = self.tag_store.get_hash_by_path(&path_str) {
                if old_hash != content_hash {
                    {
                        let mut engine =
                            self.search_engine.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = engine.delete_by_hash(&old_hash);
                    }
                    let _ = self.tag_store.delete_document_by_path(&path_str);
                }
            }

            let file_name =
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
            let doc_id = format!("{}{}", content_hash, file_type);
            let modified_ts = *mtime as i64;
            let tags = self
                .tag_store
                .get_tags_for_document(&content_hash)
                .unwrap_or_default()
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<String>>();

            if let Err(e) = self.tag_store.upsert_document(
                &content_hash,
                &path_str,
                &file_type,
                *size as i64,
                modified_ts,
            ) {
                error!("Batch upsert error for {}: {}", path_str, e);
                continue;
            }

            {
                let mut engine = self.search_engine.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(e) = engine.index_document(
                    &doc_id,
                    path,
                    &file_name,
                    &extracted.text,
                    &file_type,
                    modified_ts,
                    &content_hash,
                    &tags,
                ) {
                    error!("Batch index error for {}: {}", path_str, e);
                    continue;
                }
            }

            processed += 1;
            self.pending_count += 1;
            let _ = self.progress_tx.send(IndexerProgress::Progress { processed });
        }

        // Commit after each batch
        if self.pending_count > 0 {
            if let Err(e) = self.commit() {
                error!("Batch commit error: {}", e);
            }
            self.pending_count = 0;
        }
        processed
    }

    /// Process a file create/modify event.
    fn process_upsert(
        &mut self,
        path: &Path,
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
            debug!("Skipping {} (unchanged metadata)", path_str);
            return Ok(());
        }

        // Extract text
        let extracted = match stages::run_chain(path, stages) {
            Ok(Some(content)) => content,
            Ok(None) => {
                // No extractor handles this file type — log and skip.
                // For PDFs, this typically means pdfium.dll is not available.
                tracing::warn!(
                    "No extractor available for {} — file will not be indexed",
                    path.display()
                );
                return Ok(());
            }
            Err(e) => {
                // Log error but continue
                warn!("Extraction failed for {}: {}", path.display(), e);
                return Err(e);
            }
        };

        // Compute content hash from extracted text (not raw bytes).
        // This avoids reading the file twice and correctly deduplicates
        // documents with identical text content regardless of metadata variation.
        let file_type = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let content_hash = {
            let mut hasher = blake3::Hasher::new();
            hasher.update(extracted.text.as_bytes());
            hasher.update(file_type.as_bytes());
            hasher.finalize().to_hex().to_string()
        };

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        // Clean up old entries before inserting new ones.
        // If the file's content changed, the old content_hash differs from the
        // new one. Both SQLite (PK=content_hash) and Tantivy (stored by hash)
        // would accumulate duplicate entries without this cleanup.
        if let Ok(Some(old_hash)) = self.tag_store.get_hash_by_path(&path_str) {
            if old_hash != content_hash {
                // Remove old Tantivy document
                {
                    let mut engine = self.search_engine.lock().unwrap_or_else(|e| e.into_inner());
                    if let Err(e) = engine.delete_by_hash(&old_hash) {
                        warn!("Failed to delete old Tantivy doc {}: {}", old_hash, e);
                    }
                }
                // Remove old SQLite row (different PK = different row)
                if let Err(e) = self.tag_store.delete_document_by_path(&path_str) {
                    warn!("Failed to delete old SQLite row for {}: {}", path_str, e);
                }
            }
        }

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

        // Write SQLite FIRST, then Tantivy.
        // If crash between writes: SQLite has doc, Tantivy doesn't → next file event
        // triggers re-indexing (metadata fast-path skip fails because mtime unchanged
        // but Tantivy won't have the doc, so search returns stale results until next event).
        // Reconciliation on startup backfills any Tantivy-missing docs.
        self.tag_store.upsert_document(
            &content_hash,
            &path_str,
            &file_type,
            size as i64,
            modified_ts,
        )?;

        {
            let mut engine = self.search_engine.lock().unwrap_or_else(|e| e.into_inner());
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
    fn process_delete(&mut self, path: &Path) -> anyhow::Result<()> {
        let path_str = path.display().to_string();

        // Look up content hash from SQLite
        if let Some(content_hash) = self.tag_store.get_hash_by_path(&path_str)? {
            // Remove from Tantivy
            {
                let mut engine = self.search_engine.lock().unwrap_or_else(|e| e.into_inner());
                engine.delete_by_hash(&content_hash)?;
            }
            // Remove from SQLite
            self.tag_store.delete_document_by_path(&path_str)?;
        }

        Ok(())
    }

    /// Commit pending Tantivy changes.
    fn commit(&mut self) -> anyhow::Result<()> {
        let mut engine = self.search_engine.lock().unwrap_or_else(|e| e.into_inner());
        engine.commit()?;
        Ok(())
    }
}

/// Run reconciliation on startup: ensure Tantivy and SQLite are consistent.
/// Backfills SQLite rows for Tantivy documents that are missing them (crashes).
pub fn reconcile(engine: Arc<std::sync::Mutex<SearchEngine>>, tag_store: &TagStore) {
    info!("Running startup reconciliation...");

    // Garbage collect stale segments
    if let Err(e) = {
        let mut eng = engine.lock().unwrap_or_else(|e| e.into_inner());
        eng.garbage_collect()
    } {
        warn!("Garbage collection during reconciliation: {}", e);
    }

    // Batch-load all known content hashes from SQLite into memory.
    // This avoids per-document SELECT COUNT(*) queries (was O(n) SQLite calls).
    let known_hashes: std::collections::HashSet<String> = tag_store
        .with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT content_hash FROM documents")?;
            let hashes = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            Ok(hashes)
        })
        .unwrap_or_default();

    // Iterate all Tantivy documents and verify SQLite presence
    let eng = engine.lock().unwrap_or_else(|e| e.into_inner());
    let searcher = eng.reader.searcher();
    let mut backfill_count: usize = 0;

    for (segment_ord, segment_reader) in searcher.segment_readers().iter().enumerate() {
        for doc_id in 0u32..segment_reader.max_doc() {
            if segment_reader.is_deleted(doc_id) {
                continue;
            }

            let doc_addr = DocAddress::new(segment_ord as u32, doc_id);
            let Ok(doc) = searcher.doc::<tantivy::TantivyDocument>(doc_addr) else {
                continue;
            };

            let file_path = doc
                .get_first(eng.fields.file_path)
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let content_hash = doc
                .get_first(eng.fields.content_hash)
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let file_type = doc
                .get_first(eng.fields.file_type)
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if content_hash.is_empty() {
                continue;
            }

            // Check if file still exists on disk
            if !file_path.is_empty() && !std::path::Path::new(file_path).exists() {
                continue;
            }

            // O(1) in-memory lookup instead of per-document SQLite query
            if !known_hashes.contains(content_hash) {
                // Backfill: insert into SQLite from Tantivy stored fields
                let file_size = if file_path.is_empty() {
                    0i64
                } else {
                    std::fs::metadata(file_path)
                        .map(|m| m.len() as i64)
                        .unwrap_or(0)
                };
                let modified_ts = doc
                    .get_first(eng.fields.modified_ts)
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);

                if let Err(e) = tag_store.upsert_document(
                    content_hash,
                    file_path,
                    file_type,
                    file_size,
                    modified_ts,
                ) {
                    warn!("Reconciliation backfill failed for {}: {}", content_hash, e);
                } else {
                    backfill_count += 1;
                }
            }
        }
    }

    if backfill_count > 0 {
        info!(
            "Reconciliation backfilled {} documents to SQLite",
            backfill_count
        );
    }

    info!("Reconciliation complete");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags::store::TagStore;
    use rusqlite::Connection;
    use std::sync::Arc;

    fn setup_tag_store() -> (TagStore, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");

        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (
                content_hash TEXT PRIMARY KEY,
                file_path   TEXT NOT NULL,
                file_type   TEXT NOT NULL,
                file_size   INTEGER NOT NULL DEFAULT 0,
                modified_ts INTEGER NOT NULL DEFAULT 0,
                indexed_at  TEXT NOT NULL DEFAULT '',
                last_error  TEXT
            );
            CREATE TABLE tags (
                id   INTEGER PRIMARY KEY,
                name TEXT UNIQUE NOT NULL
            );
            CREATE TABLE document_tags (
                content_hash TEXT NOT NULL REFERENCES documents(content_hash) ON DELETE CASCADE,
                tag_id      INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                PRIMARY KEY (content_hash, tag_id)
            );",
        )
        .unwrap();

        (TagStore::new_for_test(conn), dir)
    }

    #[test]
    fn metadata_fast_path_skips_unchanged_file() {
        let (store, _dir) = setup_tag_store();

        let path = "/test/doc.pdf";
        let size: u64 = 1024;
        let mtime: u64 = 1700000000;

        // First check: not indexed yet
        assert!(
            !store
                .already_indexed_by_metadata(path, size, mtime)
                .unwrap(),
            "Should not be indexed before upsert"
        );

        // Index the document
        store
            .upsert_document("hash1", path, "pdf", size as i64, mtime as i64)
            .unwrap();

        // Same metadata should report as already indexed
        assert!(
            store
                .already_indexed_by_metadata(path, size, mtime)
                .unwrap(),
            "Same (path, size, mtime) should be detected as unchanged"
        );

        // Different size should NOT be detected as unchanged
        assert!(
            !store
                .already_indexed_by_metadata(path, size + 1, mtime)
                .unwrap(),
            "Different size should trigger re-index"
        );

        // Different mtime should NOT be detected as unchanged
        assert!(
            !store
                .already_indexed_by_metadata(path, size, mtime + 1)
                .unwrap(),
            "Different mtime should trigger re-index"
        );
    }

    #[test]
    fn content_hash_dedup_skips_duplicate() {
        let (store, _dir) = setup_tag_store();

        let hash = "abc123def456";

        // Not yet indexed
        assert!(!store.already_indexed_by_hash(hash).unwrap());

        // Index at first path
        store
            .upsert_document(hash, "/first/path/doc.pdf", "pdf", 2048, 1700000000)
            .unwrap();

        // Now detected as duplicate
        assert!(store.already_indexed_by_hash(hash).unwrap());

        // Update path for existing hash (dedup: new path, same content)
        store.update_path(hash, "/second/path/copy.pdf").unwrap();

        // Old path should no longer be in DB for this hash
        let old_hash = store.get_hash_by_path("/first/path/doc.pdf").unwrap();
        assert!(old_hash.is_none());

        // New path should map to same hash
        let new_hash = store
            .get_hash_by_path("/second/path/copy.pdf")
            .unwrap()
            .unwrap();
        assert_eq!(new_hash, hash);
    }

    #[test]
    fn failed_file_logged_to_last_error() {
        let (store, _dir) = setup_tag_store();

        // Insert a document and verify last_error column exists and can be set
        store
            .upsert_document("hash1", "/test/bad.pdf", "pdf", 0, 1700000000)
            .unwrap();

        // Set last_error manually (simulating what the pipeline would do for a corrupt file)
        store
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE documents SET last_error = ?1 WHERE content_hash = ?2",
                    rusqlite::params!["Extraction failed: corrupt PDF header", "hash1"],
                )?;

                // Verify last_error was stored
                let error: String = conn.query_row(
                    "SELECT last_error FROM documents WHERE content_hash = ?1",
                    rusqlite::params!["hash1"],
                    |row| row.get(0),
                )?;
                assert!(error.contains("corrupt PDF header"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn shutdown_commits_pending() {
        use crate::app::TagUpdate;
        use crate::search::engine::SearchEngine;
        use crate::watcher::watcher::IndexerMessage;

        let (store, _dir) = setup_tag_store();

        // Create a temp-search engine
        let index_dir = tempfile::TempDir::new().unwrap();
        let schema = crate::search::schema::build_schema();
        let fields = crate::search::schema::SchemaFields::from_schema(&schema);
        let index = tantivy::Index::create_in_dir(index_dir.path(), schema.clone()).unwrap();
        let tokenizer = tantivy::tokenizer::TextAnalyzer::builder(
            tantivy::tokenizer::SimpleTokenizer::default(),
        )
        .filter(tantivy::tokenizer::LowerCaser)
        .build();
        index.tokenizers().register("body", tokenizer);
        let writer = index.writer(50_000_000).unwrap();
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()
            .unwrap();

        let engine = SearchEngine {
            index,
            schema,
            fields,
            reader,
            writer,
        };
        let engine = Arc::new(std::sync::Mutex::new(engine));

        // Channels
        let (msg_tx, msg_rx) = crossbeam::channel::bounded::<IndexerMessage>(100);
        let (tag_tx, tag_rx) = crossbeam::channel::bounded::<TagUpdate>(100);
        let (progress_tx, _progress_rx) = crossbeam::channel::bounded::<IndexerProgress>(100);

        // Create a temp file that exists on disk (needed for process_upsert)
        let tmp_file_dir = tempfile::TempDir::new().unwrap();
        let file_path = tmp_file_dir.path().join("shutdown_test.txt");
        std::fs::write(&file_path, "document content for shutdown test").unwrap();
        let metadata = std::fs::metadata(&file_path).unwrap();

        // Send an Upsert message through the channel so the pipeline processes it
        msg_tx
            .send(IndexerMessage::Upsert {
                path: file_path.clone(),
                mtime: metadata
                    .modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                size: metadata.len(),
            })
            .unwrap();

        // Drop senders to signal shutdown AFTER the upsert message is in the queue
        drop(msg_tx);
        drop(tag_tx);

        // Create pipeline — it will process the Upsert then hit Disconnected
        let mut pipeline = Pipeline::new(
            engine.clone(),
            store,
            PathBuf::from("/test"),
            msg_rx,
            tag_rx,
            progress_tx,
        );

        // Run should process the document, then shutdown and commit
        pipeline.run();

        // After shutdown, the document should be committed.
        // Reader uses ReloadPolicy::Manual — must reload to see committed docs.
        {
            let mut eng = engine.lock().unwrap_or_else(|e| e.into_inner());
            eng.reload().unwrap();
            let count = eng.doc_count().unwrap();
            assert!(
                count > 0,
                "Document should be committed after pipeline shutdown"
            );
        }
    }
}

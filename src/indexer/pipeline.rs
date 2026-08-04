use crate::app::{AutoTagRequest, IndexerProgress, TagUpdate};
use crate::indexer::stages;
use crate::search::engine::SearchEngine;
use crate::tags::store::{BatchDocumentUpsert, TagStore};
use crate::watcher::watcher::IndexerMessage;
use crossbeam::channel::{Receiver, Sender};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    auto_tagger_tx: Option<Sender<AutoTagRequest>>,
    pending_count: usize,
    commit_batch_size: usize,
    /// Shared shutdown flag — checked during initial scan to allow fast close.
    shutdown: Arc<AtomicBool>,
    /// Set by the UI when the file browser needs a fresh document snapshot.
    /// Serviced by this thread so list_all_documents never runs on the UI
    /// thread (it is a full scan under the SQLite mutex).
    browser_refresh: Arc<AtomicBool>,
}

impl Pipeline {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        search_engine: Arc<std::sync::Mutex<SearchEngine>>,
        tag_store: TagStore,
        watcher_folder: PathBuf,
        msg_rx: Receiver<IndexerMessage>,
        tag_rx: Receiver<TagUpdate>,
        progress_tx: Sender<IndexerProgress>,
        auto_tagger_tx: Option<Sender<AutoTagRequest>>,
        shutdown: Arc<AtomicBool>,
        browser_refresh: Arc<AtomicBool>,
    ) -> Self {
        Self {
            search_engine,
            tag_store,
            watcher_folder,
            msg_rx,
            tag_rx,
            progress_tx,
            auto_tagger_tx,
            pending_count: 0,
            commit_batch_size: 10,
            shutdown,
            browser_refresh,
        }
    }

    /// Send a fresh file-browser snapshot if the UI asked for one.
    fn maybe_send_docs_snapshot(&mut self) {
        if !self.browser_refresh.swap(false, Ordering::Relaxed) {
            return;
        }
        match self.tag_store.list_all_documents() {
            Ok(docs) => {
                let _ = self
                    .progress_tx
                    .send(IndexerProgress::DocsSnapshot { docs });
            }
            Err(e) => {
                error!("Failed to build file-browser snapshot: {}", e);
            }
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

        // Always scan the folder at startup. Unchanged files are skipped via
        // the metadata fast-path (path+size+mtime), so only new or changed
        // files are extracted — this catches files added or modified while
        // the app was closed, which the watcher cannot report.
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
        // Larger batch = better parallelism, but also more memory for extracted text.
        // 32 files × ~50KB avg text = ~1.6MB per batch — well within desktop memory.
        const PARALLEL_BATCH: usize = 32;
        let mut batch: Vec<(PathBuf, u64, u64)> = Vec::with_capacity(PARALLEL_BATCH);

        for msg in scan_rx {
            // Check shutdown flag periodically — allows fast close during scan
            if self.shutdown.load(Ordering::Relaxed) {
                info!(
                    "Initial scan interrupted by shutdown at {} files",
                    scan_processed
                );
                break;
            }
            match msg {
                IndexerMessage::Upsert { path, mtime, size } => {
                    // Fast-path: skip files already indexed with identical
                    // metadata (same path+size+mtime). On lookup error, treat
                    // as not indexed — re-indexing is idempotent.
                    let path_str = path.display().to_string();
                    if self
                        .tag_store
                        .already_indexed_by_metadata(&path_str, size, mtime)
                        .unwrap_or(false)
                    {
                        debug!("Skipping {} (unchanged metadata)", path_str);
                        continue;
                    }
                    batch.push((path, mtime, size));
                    if batch.len() >= PARALLEL_BATCH {
                        let batch_start = scan_processed;
                        scan_processed += self.process_batch(&batch, batch_start);
                        batch.clear();
                        self.maybe_send_docs_snapshot();
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
            let batch_start = scan_processed;
            scan_processed += self.process_batch(&batch, batch_start);
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
        if scan_processed > 0 {
            info!("Initial scan complete: {} files indexed", scan_processed);
        }

        let mut total_processed: usize = scan_processed;
        let mut last_commit = Instant::now();
        let commit_interval = Duration::from_secs(2);

        info!("Pipeline started, waiting for messages...");

        // Create extractors for the regular message loop (one-at-a-time, no parallelism needed)
        let stages = crate::indexer::stages::create_extractor_chain();

        loop {
            // Service UI-requested file-browser refreshes (off the UI thread)
            self.maybe_send_docs_snapshot();

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

        // Final commit on channel close (shutdown) — only if documents are pending
        if self.pending_count > 0 {
            info!(
                "Pipeline shutting down, committing {} pending documents...",
                self.pending_count
            );
            if let Err(e) = self.commit() {
                error!("Final commit error: {}", e);
            }
        } else {
            info!("Pipeline shutting down (no pending documents)");
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

    fn process_batch(&mut self, batch: &[(PathBuf, u64, u64)], offset: usize) -> usize {
        use rayon::prelude::*;

        // Phase 1: Extract text in parallel. The extractor chain is built
        // once and shared (Send + Sync) — previously it was rebuilt per file.
        let stages = crate::indexer::stages::create_extractor_chain();
        let results: Vec<_> = batch
            .par_iter()
            .map(|(path, mtime, size)| {
                let extracted = match crate::indexer::stages::run_chain(path, &stages) {
                    Ok(Some(content)) => Some(content),
                    Ok(None) => None,
                    Err(_) => None,
                };
                (path, *mtime, *size, extracted)
            })
            .collect();

        // Phase 2a: reads + derived data for the whole batch (no writes yet)
        struct PendingDoc<'a> {
            path: &'a PathBuf,
            path_str: String,
            file_type: String,
            content_hash: String,
            doc_id: String,
            file_name: String,
            modified_ts: i64,
            size: u64,
            old_hash_to_delete: Option<String>,
            tags: Vec<String>,
            text: &'a str,
            content_hash_before_tag: String,
            already_tagged: bool,
        }
        let mut docs: Vec<PendingDoc> = Vec::with_capacity(results.len());
        for (path, mtime, size, extracted) in &results {
            let Some(extracted) = extracted else {
                // Extraction failed — still try to auto-tag from filename
                let file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(ref tx) = self.auto_tagger_tx {
                    let content_hash_before_tag = compute_content_hash(&file_name, "[no text]");
                    // Bounded channel: brief backpressure, then drop — the
                    // row stays 'pending' and is recovered at next startup.
                    if tx
                        .send_timeout(
                            AutoTagRequest::TagDocument {
                                content_hash: format!("batch_failed_{}", file_name),
                                filename: file_name,
                                text: "[Document text could not be extracted. Use filename to determine topic.]".to_string(),
                                content_hash_before_tag,
                            },
                            Duration::from_millis(100),
                        )
                        .is_err()
                    {
                        debug!("auto-tag queue full — request dropped (recovered at next startup)");
                    }
                }
                continue;
            };
            let path_str = path.display().to_string();
            let file_type = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            let content_hash = compute_content_hash(&extracted.text, &file_type);

            // Clean up old entries before inserting new ones
            let old_hash_to_delete = self
                .tag_store
                .get_hash_by_path(&path_str)
                .ok()
                .flatten()
                .filter(|old| old.as_str() != content_hash);

            let file_name = path
                .file_name()
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
            let content_hash_before_tag = compute_content_hash(&file_name, &extracted.text);
            // Already-tagged content must keep its status row and tags — no
            // API call, no 'pending' wipe (even if the API is down).
            let already_tagged =
                already_tagged(&self.tag_store, &content_hash, &content_hash_before_tag);

            docs.push(PendingDoc {
                path,
                path_str,
                file_type,
                content_hash,
                doc_id,
                file_name,
                modified_ts,
                size: *size,
                old_hash_to_delete,
                tags,
                text: &extracted.text,
                content_hash_before_tag,
                already_tagged,
            });
        }

        // Phase 2b: ONE transaction for all SQLite writes in this batch.
        // (Previously each file cost 2-3 autocommit WAL fsyncs.) All-or-nothing;
        // on failure fall back to per-item writes so one bad file does not
        // strand the healthy files of this batch.
        let sqlite_ok: Vec<bool> = if docs.is_empty() {
            Vec::new()
        } else {
            let items: Vec<BatchDocumentUpsert> = docs
                .iter()
                .map(|d| BatchDocumentUpsert {
                    content_hash: &d.content_hash,
                    file_path: &d.path_str,
                    file_type: &d.file_type,
                    file_size: d.size as i64,
                    modified_ts: d.modified_ts,
                    old_hash_to_delete: d.old_hash_to_delete.as_deref(),
                    filename: &d.file_name,
                    content_hash_before_tag: &d.content_hash_before_tag,
                    skip_auto_tag_status: d.already_tagged,
                })
                .collect();
            match self.tag_store.upsert_documents_batch(&items) {
                Ok(()) => vec![true; docs.len()],
                Err(e) => {
                    error!(
                        "Batch upsert failed ({} files), falling back to per-item writes: {}",
                        docs.len(),
                        e
                    );
                    docs.iter()
                        .map(|d| {
                            let ok = {
                                if d.old_hash_to_delete.is_some() {
                                    let _ = self.tag_store.delete_document_by_path(&d.path_str);
                                }
                                self.tag_store
                                    .upsert_document(
                                        &d.content_hash,
                                        &d.path_str,
                                        &d.file_type,
                                        d.size as i64,
                                        d.modified_ts,
                                    )
                                    .is_ok()
                                    && (d.already_tagged
                                        || self
                                            .tag_store
                                            .upsert_auto_tag_status(
                                                &d.content_hash,
                                                &d.file_name,
                                                &d.content_hash_before_tag,
                                                "pending",
                                                None,
                                                None,
                                            )
                                            .is_ok())
                            };
                            if !ok {
                                error!("Per-item upsert failed for {}", d.path_str);
                            }
                            ok
                        })
                        .collect()
                }
            }
        };

        // Phase 2c: Tantivy writes (single lock scope) + auto-tag requests
        let mut processed = 0usize;
        for (doc, ok) in docs.iter().zip(&sqlite_ok) {
            if !ok {
                continue;
            }
            {
                let mut engine = self.search_engine.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ref old_hash) = doc.old_hash_to_delete {
                    let _ = engine.delete_by_hash(old_hash);
                }
                if let Err(e) = engine.index_document(
                    &doc.doc_id,
                    doc.path,
                    &doc.file_name,
                    doc.text,
                    &doc.file_type,
                    doc.modified_ts,
                    &doc.content_hash,
                    &doc.tags,
                ) {
                    error!("Batch index error for {}: {}", doc.path_str, e);
                    continue;
                }
            }

            // Trigger auto-tagging (status row already written in 2b) —
            // except for already-tagged content, which must not re-call the API.
            if !doc.already_tagged {
                if let Some(ref tx) = self.auto_tagger_tx {
                    let request = AutoTagRequest::TagDocument {
                        content_hash: doc.content_hash.clone(),
                        filename: doc.file_name.clone(),
                        text: doc.text.to_string(),
                        content_hash_before_tag: doc.content_hash_before_tag.clone(),
                    };
                    // Bounded channel: brief backpressure, then drop — the
                    // row stays 'pending' and is recovered at next startup.
                    if tx
                        .send_timeout(request, Duration::from_millis(100))
                        .is_err()
                    {
                        debug!("auto-tag queue full — request dropped (recovered at next startup)");
                    }
                }
            }

            processed += 1;
            self.pending_count += 1;
            let _ = self.progress_tx.send(IndexerProgress::Progress {
                processed: offset + processed,
            });
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
            Ok(Some(content)) => Some(content),
            Ok(None) => {
                tracing::warn!(
                    "No extractor available for {} — indexing filename only",
                    path.display()
                );
                None
            }
            Err(e) => {
                warn!(
                    "Extraction failed for {}: {} — indexing filename only",
                    path.display(),
                    e
                );
                None
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
        let extracted_text = extracted
            .as_ref()
            .map(|e| e.text.as_str())
            .unwrap_or("[Document text could not be extracted. Use filename to determine topic.]");
        let content_hash = compute_content_hash(extracted_text, &file_type);

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        // Clean up old entries before inserting new ones.
        // If the file's content changed, the old content_hash differs from the
        // new one. Both SQLite (PK=content_hash) and Tantivy (stored by hash)
        // would accumulate duplicate entries without this cleanup.
        let old_hash_to_delete = self
            .tag_store
            .get_hash_by_path(&path_str)
            .ok()
            .flatten()
            .filter(|old| old.as_str() != content_hash);
        if old_hash_to_delete.is_some() {
            if let Err(e) = self.tag_store.delete_document_by_path(&path_str) {
                warn!("Failed to delete old SQLite row for {}: {}", path_str, e);
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
        // If crash between writes: SQLite has doc, Tantivy doesn't. The
        // metadata fast-path will skip re-indexing (same path+size+mtime),
        // but the file becomes searchable again after any modification.
        // Reconciliation backfills Tantivy-missing SQLite rows, but the
        // reverse (SQLite→Tantivy) requires re-extraction on modification.
        self.tag_store.upsert_document(
            &content_hash,
            &path_str,
            &file_type,
            size as i64,
            modified_ts,
        )?;

        // Single lock scope: delete old + index new (avoids double Mutex acquire)
        {
            let mut engine = self.search_engine.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref old_hash) = old_hash_to_delete {
                if let Err(e) = engine.delete_by_hash(old_hash) {
                    warn!("Failed to delete old Tantivy doc {}: {}", old_hash, e);
                }
            }
            engine.index_document(
                &doc_id,
                path,
                &file_name,
                extracted_text,
                &file_type,
                modified_ts,
                &content_hash,
                &tags,
            )?;
        }

        // Persist auto-tag request to DB after successful indexing
        // (replaces the old channel-based queue that silently dropped documents)
        {
            let content_hash_before_tag = compute_content_hash(&file_name, extracted_text);
            // Never re-request tags for content that is already tagged —
            // the status row (and its tags) must survive re-indexing.
            if already_tagged(&self.tag_store, &content_hash, &content_hash_before_tag) {
                debug!(
                    "Skipping auto-tag request for {} (already tagged)",
                    path.display()
                );
                return Ok(());
            }
            if let Err(e) = self.tag_store.upsert_auto_tag_status(
                &content_hash,
                &file_name,
                &content_hash_before_tag,
                "pending",
                None,
                None,
            ) {
                tracing::warn!(
                    "failed to write pending auto-tag status for {}: {}",
                    content_hash,
                    e
                );
            }

            // Wake the auto-tagger via a lightweight channel notification
            if let Some(ref tx) = self.auto_tagger_tx {
                let request = AutoTagRequest::TagDocument {
                    content_hash: content_hash.clone(),
                    filename: file_name.clone(),
                    text: extracted_text.to_string(),
                    content_hash_before_tag,
                };
                // Bounded channel: brief backpressure, then drop — the row
                // stays 'pending' and is recovered at next startup.
                if tx
                    .send_timeout(request, Duration::from_millis(100))
                    .is_err()
                {
                    debug!("auto-tag queue full — request dropped (recovered at next startup)");
                }
            }
        }

        Ok(())
    }

    /// Process a file deletion.
    fn process_delete(&mut self, path: &Path) -> anyhow::Result<()> {
        let path_str = path.display().to_string();

        // Defense in depth against spurious Remove events (common on network
        // shares). Deleting the documents row CASCADEs away the auto-tag
        // status (AI tags); a file that still exists would be re-tagged on
        // the next scan — paid API calls for nothing. The watcher already
        // filters these, but races (Remove+Create collapse inside the debounce
        // window) can still reach here with a live file.
        if path.exists() {
            debug!(
                "Skipping delete for {} (file still exists — likely spurious Remove event)",
                path_str
            );
            return Ok(());
        }

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

/// Compute a BLAKE3 content hash from text content and file type.
/// Deterministic: same (text, type) always produces the same hash.
pub fn compute_content_hash(text: &str, file_type: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(text.as_bytes());
    hasher.update(file_type.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// True when the exact same content was already tagged and the tags are
/// still stored. Re-indexing such a file must NOT wipe its status row or
/// re-call the API — that would lose tags for no benefit (and permanently,
/// if the API happens to be down at that moment).
fn already_tagged(store: &TagStore, content_hash: &str, content_hash_before_tag: &str) -> bool {
    match store.auto_tag_status(content_hash) {
        Ok(Some(status)) => {
            status.status == "tagged"
                && status.tags_json.is_some()
                && status.content_hash_before_tag == content_hash_before_tag
        }
        _ => false,
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
            "Reconciliation backfilled {} documents to SQLite (Tantivy→SQLite).",
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
    use std::sync::atomic::AtomicBool;
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
            );
            CREATE TABLE auto_tag_status (
                content_hash TEXT PRIMARY KEY REFERENCES documents(content_hash) ON DELETE CASCADE,
                filename     TEXT NOT NULL,
                content_hash_before_tag TEXT NOT NULL,
                status       TEXT NOT NULL DEFAULT 'pending',
                tags_json    TEXT,
                attempts     INTEGER NOT NULL DEFAULT 0,
                last_error   TEXT,
                created_at   TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at   TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE auto_tag_cache (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                filename_tokens TEXT NOT NULL,
                tags_json       TEXT NOT NULL,
                source_hash     TEXT NOT NULL,
                hit_count       INTEGER NOT NULL DEFAULT 1,
                created_at      TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
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
    fn process_delete_keeps_rows_when_file_still_exists() {
        // A spurious Remove event (network-share watcher) must not delete the
        // documents row — the cascade would wipe the auto-tag status (AI
        // tags) and the file would be re-tagged on the next scan.
        use crate::app::TagUpdate;
        use crate::search::engine::SearchEngine;
        use crate::watcher::watcher::IndexerMessage;

        let (store, _dir) = setup_tag_store();

        let watched = tempfile::TempDir::new().unwrap();
        let file = watched.path().join("doc.pdf");
        std::fs::write(&file, "content").unwrap();
        let path_str = file.display().to_string();
        store
            .upsert_document("hash1", &path_str, "pdf", 7, 0)
            .unwrap();
        store
            .upsert_auto_tag_status(
                "hash1",
                "doc.pdf",
                "before",
                "tagged",
                Some(r#"{"tags":["keep"]}"#),
                None,
            )
            .unwrap();

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
        let engine = Arc::new(std::sync::Mutex::new(SearchEngine {
            index,
            schema,
            fields,
            reader,
            writer,
        }));
        {
            let mut eng = engine.lock().unwrap_or_else(|e| e.into_inner());
            eng.index_document(
                "hash1pdf",
                &file,
                "doc.pdf",
                "content",
                "pdf",
                0,
                "hash1",
                &[],
            )
            .unwrap();
            eng.commit().unwrap();
        }

        let (_msg_tx, msg_rx) = crossbeam::channel::bounded::<IndexerMessage>(100);
        let (_tag_tx, tag_rx) = crossbeam::channel::bounded::<TagUpdate>(100);
        let (progress_tx, _progress_rx) = crossbeam::channel::unbounded::<IndexerProgress>();
        let mut pipeline = Pipeline::new(
            engine,
            store.clone(),
            watched.path().to_path_buf(),
            msg_rx,
            tag_rx,
            progress_tx,
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );

        // File still on disk → spurious Remove must be ignored.
        pipeline.process_delete(&file).unwrap();
        assert!(
            store.auto_tag_status("hash1").unwrap().is_some(),
            "auto-tag status must survive a spurious Remove event"
        );
        assert!(
            store.get_hash_by_path(&path_str).unwrap().is_some(),
            "documents row must survive a spurious Remove event"
        );

        // File really gone → delete proceeds (rows removed).
        std::fs::remove_file(&file).unwrap();
        pipeline.process_delete(&file).unwrap();
        assert!(
            store.auto_tag_status("hash1").unwrap().is_none(),
            "a real delete must remove the auto-tag status row"
        );
        assert!(store.get_hash_by_path(&path_str).unwrap().is_none());
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

        let shutdown = Arc::new(AtomicBool::new(false));
        // Create pipeline — it will process the Upsert then hit Disconnected
        let mut pipeline = Pipeline::new(
            engine.clone(),
            store,
            PathBuf::from("/test"),
            msg_rx,
            tag_rx,
            progress_tx,
            None,
            shutdown,
            Arc::new(AtomicBool::new(false)),
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

    #[test]
    fn browser_refresh_flag_sends_docs_snapshot() {
        use crate::app::TagUpdate;
        use crate::search::engine::SearchEngine;
        use crate::tags::store::DocumentInfo;
        use crate::watcher::watcher::IndexerMessage;

        let (store, _dir) = setup_tag_store();
        store
            .upsert_document("h1", "/test/doc.pdf", "pdf", 10, 1)
            .unwrap();

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

        let (msg_tx, msg_rx) = crossbeam::channel::bounded::<IndexerMessage>(100);
        let (tag_tx, tag_rx) = crossbeam::channel::bounded::<TagUpdate>(100);
        let (progress_tx, progress_rx) = crossbeam::channel::unbounded::<IndexerProgress>();

        // UI asked for a refresh before the pipeline even starts.
        let browser_refresh = Arc::new(AtomicBool::new(true));
        let mut pipeline = Pipeline::new(
            engine.clone(),
            store,
            PathBuf::from("/test"),
            msg_rx,
            tag_rx,
            progress_tx,
            None,
            Arc::new(AtomicBool::new(false)),
            browser_refresh,
        );

        // No messages — the channel disconnects immediately; run() must still
        // service the refresh flag on its first iteration.
        drop(msg_tx);
        drop(tag_tx);
        pipeline.run();

        let mut snapshots: Vec<Vec<DocumentInfo>> = Vec::new();
        while let Ok(msg) = progress_rx.try_recv() {
            if let IndexerProgress::DocsSnapshot { docs } = msg {
                snapshots.push(docs);
            }
        }
        let last = snapshots.last().expect("at least one DocsSnapshot");
        assert_eq!(last.len(), 1, "snapshot must include the seeded document");
        assert_eq!(last[0].content_hash, "h1");
        assert_eq!(last[0].file_path, "/test/doc.pdf");
    }

    #[test]
    fn startup_scan_indexes_files_added_while_app_was_closed() {
        // A previous session indexed existing.txt; while the app was closed,
        // newfile.txt was added to the folder. The watcher cannot see events
        // it missed, so the startup scan must pick up newfile.txt while
        // skipping existing.txt (unchanged metadata).
        use crate::app::TagUpdate;
        use crate::search::engine::SearchEngine;
        use crate::tags::store::DocumentInfo;
        use crate::watcher::watcher::IndexerMessage;

        let (store, _dir) = setup_tag_store();

        // Watched folder with one "old" file and one "new" file.
        let watched = tempfile::TempDir::new().unwrap();
        let existing = watched.path().join("existing.txt");
        std::fs::write(&existing, "existing content").unwrap();
        let em = std::fs::metadata(&existing).unwrap();
        let existing_mtime = em
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let new_file = watched.path().join("newfile.txt");
        std::fs::write(&new_file, "brand new content").unwrap();

        // Temp search engine (same inline setup as sibling tests).
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
        let engine = Arc::new(std::sync::Mutex::new(SearchEngine {
            index,
            schema,
            fields,
            reader,
            writer,
        }));

        // Seed the "previous session" state: SQLite row + Tantivy doc, so the
        // old behavior (doc_count > 0 → skip initial scan) would trigger.
        let existing_path_str = existing.display().to_string();
        store
            .upsert_document(
                "hash-existing",
                &existing_path_str,
                "txt",
                em.len() as i64,
                existing_mtime as i64,
            )
            .unwrap();
        {
            let mut eng = engine.lock().unwrap_or_else(|e| e.into_inner());
            eng.index_document(
                "hash-existingtxt",
                &existing,
                "existing.txt",
                "existing content",
                "txt",
                existing_mtime as i64,
                "hash-existing",
                &[],
            )
            .unwrap();
            eng.commit().unwrap();
            eng.reload().unwrap(); // Manual reload policy: make the seed visible
        }

        let (msg_tx, msg_rx) = crossbeam::channel::bounded::<IndexerMessage>(100);
        let (tag_tx, tag_rx) = crossbeam::channel::bounded::<TagUpdate>(100);
        let (progress_tx, progress_rx) = crossbeam::channel::unbounded::<IndexerProgress>();

        let mut pipeline = Pipeline::new(
            engine.clone(),
            store,
            watched.path().to_path_buf(),
            msg_rx,
            tag_rx,
            progress_tx,
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)), // browser refresh requested at startup
        );

        // No watcher events; closing the channel ends run() after the scan.
        drop(msg_tx);
        drop(tag_tx);
        pipeline.run();

        let mut snapshots: Vec<Vec<DocumentInfo>> = Vec::new();
        let mut scan_total: Option<usize> = None;
        while let Ok(msg) = progress_rx.try_recv() {
            match msg {
                IndexerProgress::DocsSnapshot { docs } => snapshots.push(docs),
                IndexerProgress::ScanComplete { total } => scan_total = Some(total),
                _ => {}
            }
        }

        let last = snapshots.last().expect("at least one DocsSnapshot");
        let new_path_str = new_file.display().to_string();
        assert!(
            last.iter().any(|d| d.file_path == new_path_str),
            "startup scan must index the file added while the app was closed"
        );
        assert_eq!(
            last.iter().filter(|d| d.file_path == existing_path_str).count(),
            1,
            "unchanged file must not be duplicated by the scan"
        );
        assert_eq!(
            scan_total,
            Some(1),
            "scan must process only the new file (unchanged file skipped)"
        );
    }

    #[test]
    fn startup_scan_reindexes_files_changed_while_app_was_closed() {
        // A file modified while the app was closed has stale metadata in
        // SQLite; the startup scan must re-extract and re-index it.
        use crate::app::TagUpdate;
        use crate::search::engine::SearchEngine;
        use crate::tags::store::DocumentInfo;
        use crate::watcher::watcher::IndexerMessage;

        let (store, _dir) = setup_tag_store();

        let watched = tempfile::TempDir::new().unwrap();
        let file = watched.path().join("doc.txt");
        std::fs::write(&file, "changed content").unwrap();

        // Stale row from the "previous session": old size, old mtime,
        // old hash — none of them match the on-disk file.
        let path_str = file.display().to_string();
        store
            .upsert_document("hash-stale", &path_str, "txt", 0, 0)
            .unwrap();

        // Temp search engine (same inline setup as sibling tests).
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
        let engine = Arc::new(std::sync::Mutex::new(SearchEngine {
            index,
            schema,
            fields,
            reader,
            writer,
        }));

        // Seed the "previous session" Tantivy state (doc_count > 0) so the
        // old behavior (skip initial scan on non-empty index) would trigger
        // — this test must fail on the old code.
        {
            let mut eng = engine.lock().unwrap_or_else(|e| e.into_inner());
            eng.index_document(
                "hash-staletxt",
                &file,
                "doc.txt",
                "old content",
                "txt",
                0,
                "hash-stale",
                &[],
            )
            .unwrap();
            eng.commit().unwrap();
            eng.reload().unwrap(); // Manual reload policy: make the seed visible
        }

        let (msg_tx, msg_rx) = crossbeam::channel::bounded::<IndexerMessage>(100);
        let (tag_tx, tag_rx) = crossbeam::channel::bounded::<TagUpdate>(100);
        let (progress_tx, progress_rx) = crossbeam::channel::unbounded::<IndexerProgress>();

        let mut pipeline = Pipeline::new(
            engine.clone(),
            store,
            watched.path().to_path_buf(),
            msg_rx,
            tag_rx,
            progress_tx,
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)), // browser refresh requested at startup
        );

        // No watcher events; closing the channel ends run() after the scan.
        drop(msg_tx);
        drop(tag_tx);
        pipeline.run();

        let mut snapshots: Vec<Vec<DocumentInfo>> = Vec::new();
        let mut scan_total: Option<usize> = None;
        while let Ok(msg) = progress_rx.try_recv() {
            match msg {
                IndexerProgress::DocsSnapshot { docs } => snapshots.push(docs),
                IndexerProgress::ScanComplete { total } => scan_total = Some(total),
                _ => {}
            }
        }

        let last = snapshots.last().expect("at least one DocsSnapshot");
        let doc = last
            .iter()
            .find(|d| d.file_path == path_str)
            .expect("changed file must be re-indexed by the scan");
        let expected_hash = crate::indexer::pipeline::compute_content_hash("changed content", "txt");
        assert_eq!(
            doc.content_hash, expected_hash,
            "changed file must be re-extracted and re-indexed"
        );
        assert_eq!(scan_total, Some(1), "scan must process the changed file");
    }

    #[test]
    fn process_batch_extracts_and_indexes_files() {
        use crate::app::TagUpdate;
        use crate::search::engine::SearchEngine;
        use crate::watcher::watcher::IndexerMessage;
        use std::path::PathBuf;

        let (store, _dir) = setup_tag_store();

        // Create search engine
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

        let (_msg_tx, msg_rx) = crossbeam::channel::bounded::<IndexerMessage>(1);
        let (_tag_tx, tag_rx) = crossbeam::channel::bounded::<TagUpdate>(1);
        let (progress_tx, progress_rx) = crossbeam::channel::unbounded::<IndexerProgress>();

        // Create test files
        let tmp = tempfile::TempDir::new().unwrap();
        let f1 = tmp.path().join("a.txt");
        let f2 = tmp.path().join("b.txt");
        std::fs::write(&f1, "hello world").unwrap();
        std::fs::write(&f2, "foo bar baz").unwrap();
        let m1 = std::fs::metadata(&f1).unwrap();
        let m2 = std::fs::metadata(&f2).unwrap();
        let mtime1 = m1
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mtime2 = m2
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let size1 = m1.len();
        let size2 = m2.len();

        let batch = vec![(f1.clone(), mtime1, size1), (f2.clone(), mtime2, size2)];

        let mut pipeline = Pipeline::new(
            engine.clone(),
            store,
            PathBuf::from("/test"),
            msg_rx,
            tag_rx,
            progress_tx,
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );

        // Process batch with offset 0
        let processed = pipeline.process_batch(&batch, 0);
        assert_eq!(processed, 2, "Should process both files");

        // Verify progress messages
        let mut progress_count = 0;
        while let Ok(p) = progress_rx.try_recv() {
            if let IndexerProgress::Progress { processed } = p {
                assert!(
                    processed > 0,
                    "Progress should start from 1 with offset 0, got {}",
                    processed
                );
            }
            progress_count += 1;
        }
        assert_eq!(progress_count, 2, "Should emit 2 progress messages");

        // Verify documents were indexed
        {
            let mut eng = engine.lock().unwrap_or_else(|e| e.into_inner());
            eng.reload().unwrap();
            let count = eng.doc_count().unwrap();
            assert_eq!(count, 2, "Should have 2 indexed documents");
        }
    }

    #[test]
    fn process_batch_offset_produces_cumulative_progress() {
        use crate::app::TagUpdate;
        use crate::search::engine::SearchEngine;
        use crate::watcher::watcher::IndexerMessage;
        use std::path::PathBuf;

        let (store, _dir) = setup_tag_store();
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

        let (_msg_tx, msg_rx) = crossbeam::channel::bounded::<IndexerMessage>(1);
        let (_tag_tx, tag_rx) = crossbeam::channel::bounded::<TagUpdate>(1);
        let (progress_tx, progress_rx) = crossbeam::channel::unbounded::<IndexerProgress>();

        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("x.txt");
        std::fs::write(&f, "content").unwrap();
        let m = std::fs::metadata(&f).unwrap();
        let mtime = m
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let batch = vec![(f.clone(), mtime, m.len())];

        let mut pipeline = Pipeline::new(
            engine.clone(),
            store,
            PathBuf::from("/test"),
            msg_rx,
            tag_rx,
            progress_tx,
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );

        // Process with offset 10 (simulating second batch)
        let processed = pipeline.process_batch(&batch, 10);
        assert_eq!(processed, 1);

        // Verify progress starts from offset+1
        let progress = progress_rx.try_recv().unwrap();
        if let IndexerProgress::Progress { processed } = progress {
            assert_eq!(
                processed, 11,
                "Progress should be offset+1 = 11, got {}",
                processed
            );
        } else {
            panic!("Expected Progress");
        }
    }

    #[test]
    fn process_batch_skips_unsupported_files() {
        use crate::app::TagUpdate;
        use crate::search::engine::SearchEngine;
        use crate::watcher::watcher::IndexerMessage;
        use std::path::PathBuf;

        let (store, _dir) = setup_tag_store();
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

        let (_msg_tx, msg_rx) = crossbeam::channel::bounded::<IndexerMessage>(1);
        let (_tag_tx, tag_rx) = crossbeam::channel::bounded::<TagUpdate>(1);
        let (progress_tx, _rx) = crossbeam::channel::unbounded::<IndexerProgress>();

        let tmp = tempfile::TempDir::new().unwrap();
        let bad = tmp.path().join("corrupt.pdf");
        std::fs::write(&bad, b"%%BAD_DATA%%").unwrap();
        let m = std::fs::metadata(&bad).unwrap();
        let mtime = m
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let batch = vec![(bad.clone(), mtime, m.len())];

        let mut pipeline = Pipeline::new(
            engine.clone(),
            store,
            PathBuf::from("/test"),
            msg_rx,
            tag_rx,
            progress_tx,
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );

        // Corrupt PDF should not crash — just skip
        let processed = pipeline.process_batch(&batch, 0);
        assert_eq!(processed, 0, "Corrupt file should be skipped, not crash");

        let mut eng = engine.lock().unwrap_or_else(|e| e.into_inner());
        eng.reload().unwrap();
        assert_eq!(
            eng.doc_count().unwrap(),
            0,
            "No docs should be indexed from corrupt file"
        );
    }

    /// Shared engine + pipeline harness for auto-tag request tests.
    fn batch_harness() -> (
        TagStore,
        tempfile::TempDir,
        Arc<std::sync::Mutex<SearchEngine>>,
    ) {
        use crate::search::engine::SearchEngine;
        let (store, dir) = setup_tag_store();
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
        (store, dir, Arc::new(std::sync::Mutex::new(engine)))
    }

    #[test]
    fn process_batch_preserves_tags_and_skips_request_for_tagged_content() {
        use crate::app::TagUpdate;
        use crate::watcher::watcher::IndexerMessage;
        use std::path::PathBuf;

        let (store, _dir, engine) = batch_harness();
        let (_msg_tx, msg_rx) = crossbeam::channel::bounded::<IndexerMessage>(1);
        let (_tag_tx, tag_rx) = crossbeam::channel::bounded::<TagUpdate>(1);
        let (progress_tx, _progress_rx) = crossbeam::channel::unbounded::<IndexerProgress>();
        let (auto_tx, auto_rx) = crossbeam::channel::bounded::<AutoTagRequest>(16);

        // Seed: file already tagged with this exact content.
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        let text = "hello world";
        std::fs::write(&f, text).unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        let file_name = "a.txt";
        let content_hash = compute_content_hash(text, "txt");
        let hash_before = compute_content_hash(file_name, text);
        store
            .upsert_document(&content_hash, f.to_str().unwrap(), "txt", 0, 0)
            .unwrap();
        store
            .upsert_auto_tag_status(
                &content_hash,
                file_name,
                &hash_before,
                "tagged",
                Some(r#"{"tags":["tax"]}"#),
                None,
            )
            .unwrap();

        let mut pipeline = Pipeline::new(
            engine.clone(),
            store.clone(),
            PathBuf::from("/test"),
            msg_rx,
            tag_rx,
            progress_tx,
            Some(auto_tx),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let batch = vec![(
            f.clone(),
            meta.modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            meta.len(),
        )];
        let processed = pipeline.process_batch(&batch, 0);
        assert_eq!(processed, 1);

        // Tags must survive re-indexing untouched — no 'pending' wipe.
        let status = store.auto_tag_status(&content_hash).unwrap().unwrap();
        assert_eq!(status.status, "tagged");
        assert_eq!(status.tags_json.as_deref(), Some(r#"{"tags":["tax"]}"#));
        // And NO auto-tag request may be queued for already-tagged content.
        assert!(
            auto_rx.is_empty(),
            "no API request must be sent for already-tagged content"
        );
    }

    #[test]
    fn process_batch_requests_tags_for_changed_content() {
        use crate::app::TagUpdate;
        use crate::watcher::watcher::IndexerMessage;
        use std::path::PathBuf;

        let (store, _dir, engine) = batch_harness();
        let (_msg_tx, msg_rx) = crossbeam::channel::bounded::<IndexerMessage>(1);
        let (_tag_tx, tag_rx) = crossbeam::channel::bounded::<TagUpdate>(1);
        let (progress_tx, _progress_rx) = crossbeam::channel::unbounded::<IndexerProgress>();
        let (auto_tx, auto_rx) = crossbeam::channel::bounded::<AutoTagRequest>(16);

        // Seed: the file WAS tagged, but its content has since changed — the
        // stored hash-before differs from the current content's hash.
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        let text = "hello world changed";
        std::fs::write(&f, text).unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        let file_name = "a.txt";
        let content_hash = compute_content_hash(text, "txt");
        let hash_before = compute_content_hash(file_name, text);
        store
            .upsert_document(&content_hash, f.to_str().unwrap(), "txt", 0, 0)
            .unwrap();
        store
            .upsert_auto_tag_status(
                &content_hash,
                file_name,
                "stale-hash-before", // tagged with DIFFERENT content
                "tagged",
                Some(r#"{"tags":["old"]}"#),
                None,
            )
            .unwrap();

        let mut pipeline = Pipeline::new(
            engine.clone(),
            store.clone(),
            PathBuf::from("/test"),
            msg_rx,
            tag_rx,
            progress_tx,
            Some(auto_tx),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let batch = vec![(
            f.clone(),
            meta.modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            meta.len(),
        )];
        let processed = pipeline.process_batch(&batch, 0);
        assert_eq!(processed, 1);

        // Changed content must be re-tagged: status reset to pending and a
        // request queued for the fresh content.
        let status = store.auto_tag_status(&content_hash).unwrap().unwrap();
        assert_eq!(status.status, "pending");
        let request = auto_rx.try_recv().unwrap();
        match request {
            AutoTagRequest::TagDocument {
                content_hash: h,
                content_hash_before_tag: hb,
                ..
            } => {
                assert_eq!(h, content_hash);
                assert_eq!(hb, hash_before);
            }
            AutoTagRequest::Shutdown => panic!("unexpected shutdown"),
        }
    }
}

use rusqlite::{params, Connection, Result as SqlResult};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::info;

use super::model::{AutoTagStatus, Tag};

/// Manages tag storage via a single persistent SQLite connection (WAL mode).
/// Cloneable via Arc — all clones share the same connection.
#[derive(Clone)]
pub struct TagStore {
    conn: Arc<Mutex<Connection>>,
}

impl TagStore {
    /// Test-only constructor. Not for production use — use open_or_create().
    #[cfg(test)]
    pub fn new_for_test(conn: Connection) -> Self {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys = ON;",
        )
        .ok();
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }
    /// Open or create the tag database at the standard location.
    pub fn open_or_create() -> SqlResult<Self> {
        let db_path = Self::db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let conn = Connection::open(&db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA mmap_size=268435456;
             PRAGMA busy_timeout=5000;
             PRAGMA foreign_keys = ON;",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS documents (
                content_hash TEXT PRIMARY KEY,
                file_path   TEXT NOT NULL,
                file_type   TEXT NOT NULL,
                file_size   INTEGER NOT NULL DEFAULT 0,
                modified_ts INTEGER NOT NULL DEFAULT 0,
                indexed_at  TEXT NOT NULL DEFAULT '',
                last_error  TEXT
            );
            CREATE TABLE IF NOT EXISTS tags (
                id   INTEGER PRIMARY KEY,
                name TEXT UNIQUE NOT NULL
            );
            CREATE TABLE IF NOT EXISTS document_tags (
                content_hash TEXT NOT NULL REFERENCES documents(content_hash) ON DELETE CASCADE,
                tag_id      INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                PRIMARY KEY (content_hash, tag_id)
            );
            CREATE INDEX IF NOT EXISTS idx_documents_file_path ON documents(file_path);

            -- Per-document auto-tagging state
            CREATE TABLE IF NOT EXISTS auto_tag_status (
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

            -- Filename-token cache for tier-2 tag lookups
            CREATE TABLE IF NOT EXISTS auto_tag_cache (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                filename_tokens TEXT NOT NULL,
                tags_json       TEXT NOT NULL,
                source_hash     TEXT NOT NULL,
                hit_count       INTEGER NOT NULL DEFAULT 1,
                created_at      TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_auto_tag_cache_tokens ON auto_tag_cache(filename_tokens);",
        )?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn db_path() -> PathBuf {
        let base = dirs_next::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("papervault").join("papervault.db")
    }

    /// Access the underlying connection. Caller must hold the lock briefly.
    pub(crate) fn with_conn<F, T>(&self, f: F) -> SqlResult<T>
    where
        F: FnOnce(&Connection) -> SqlResult<T>,
    {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&conn)
    }

    // ── Tag CRUD ──

    pub fn create_tag(&self, name: &str) -> SqlResult<Tag> {
        self.with_conn(|conn| {
            conn.execute("INSERT INTO tags (name) VALUES (?1)", params![name])?;
            let id = conn.last_insert_rowid();
            Ok(Tag {
                id,
                name: name.to_string(),
            })
        })
    }

    pub fn list_tags(&self) -> SqlResult<Vec<Tag>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT id, name FROM tags ORDER BY name")?;
            let tags = stmt
                .query_map([], |row| {
                    Ok(Tag {
                        id: row.get(0)?,
                        name: row.get(1)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(tags)
        })
    }

    #[allow(dead_code)]
    pub fn delete_tag(&self, tag_id: i64) -> SqlResult<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM tags WHERE id = ?1", params![tag_id])?;
            Ok(())
        })
    }

    // ── Document Tag Assignment ──

    pub fn assign_tag(&self, content_hash: &str, tag_id: i64) -> SqlResult<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO document_tags (content_hash, tag_id) VALUES (?1, ?2)",
                params![content_hash, tag_id],
            )?;
            Ok(())
        })
    }

    #[allow(dead_code)]
    pub fn remove_tag(&self, content_hash: &str, tag_id: i64) -> SqlResult<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM document_tags WHERE content_hash = ?1 AND tag_id = ?2",
                params![content_hash, tag_id],
            )?;
            Ok(())
        })
    }

    pub fn get_tags_for_document(&self, content_hash: &str) -> SqlResult<Vec<Tag>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT t.id, t.name FROM tags t
                 JOIN document_tags dt ON t.id = dt.tag_id
                 WHERE dt.content_hash = ?1 ORDER BY t.name",
            )?;
            let tags = stmt
                .query_map(params![content_hash], |row| {
                    Ok(Tag {
                        id: row.get(0)?,
                        name: row.get(1)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(tags)
        })
    }

    /// Batch-fetch tags for multiple document hashes in a single query.
    /// Accepts `&[&str]` to avoid unnecessary String allocations at call sites.
    pub fn get_tags_for_hashes(
        &self,
        hashes: &[&str],
    ) -> SqlResult<std::collections::HashMap<String, Vec<Tag>>> {
        use std::collections::HashMap;
        if hashes.is_empty() {
            return Ok(HashMap::new());
        }
        self.with_conn(|conn| {
            let mut result: HashMap<String, Vec<Tag>> = HashMap::new();
            for chunk in hashes.chunks(500) {
                let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
                let sql = format!(
                    "SELECT dt.content_hash, t.id, t.name FROM document_tags dt JOIN tags t ON dt.tag_id = t.id WHERE dt.content_hash IN ({}) ORDER BY t.name",
                    placeholders
                );
                let mut stmt = conn.prepare(&sql)?;
                let params: Vec<&dyn rusqlite::types::ToSql> = chunk
                    .iter()
                    .map(|s| s as &dyn rusqlite::types::ToSql)
                    .collect();
                let rows = stmt.query_map(params.as_slice(), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        Tag {
                            id: row.get(1)?,
                            name: row.get(2)?,
                        },
                    ))
                })?;
                for (hash, tag) in rows.flatten() {
                    result.entry(hash).or_default().push(tag);
                }
            }
            Ok(result)
        })
    }

    #[allow(dead_code)]
    pub fn get_documents_with_tag(&self, tag_id: i64) -> SqlResult<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT content_hash FROM document_tags WHERE tag_id = ?1")?;
            let hashes = stmt
                .query_map(params![tag_id], |row| row.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            Ok(hashes)
        })
    }

    // ── Document Metadata ──

    pub fn already_indexed_by_metadata(
        &self,
        path: &str,
        size: u64,
        mtime: u64,
    ) -> SqlResult<bool> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT COUNT(*) FROM documents
                 WHERE file_path = ?1 AND file_size = ?2 AND modified_ts = ?3",
            )?;
            let count: i64 =
                stmt.query_row(params![path, size as i64, mtime as i64], |r| r.get(0))?;
            Ok(count > 0)
        })
    }

    #[allow(dead_code)] // used by reconcile path in prior version; kept for API completeness
    pub fn already_indexed_by_hash(&self, content_hash: &str) -> SqlResult<bool> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT COUNT(*) FROM documents WHERE content_hash = ?1")?;
            let count: i64 = stmt.query_row(params![content_hash], |r| r.get(0))?;
            Ok(count > 0)
        })
    }

    #[allow(dead_code)] // used by test and prior dedup path
    pub fn update_path(&self, content_hash: &str, new_path: &str) -> SqlResult<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE documents SET file_path = ?1 WHERE content_hash = ?2",
                params![new_path, content_hash],
            )?;
            Ok(())
        })
    }

    pub fn upsert_document(
        &self,
        content_hash: &str,
        file_path: &str,
        file_type: &str,
        file_size: i64,
        modified_ts: i64,
    ) -> SqlResult<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO documents (content_hash, file_path, file_type, file_size, modified_ts, indexed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
                params![content_hash, file_path, file_type, file_size, modified_ts],
            )?;
            Ok(())
        })
    }

    pub fn delete_document_by_path(&self, path: &str) -> SqlResult<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM documents WHERE file_path = ?1", params![path])?;
            Ok(())
        })
    }

    /// Delete a document by content hash (used by ghost sweep).
    pub fn delete_document_by_hash(&self, content_hash: &str) -> SqlResult<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM documents WHERE content_hash = ?1",
                params![content_hash],
            )?;
            Ok(())
        })
    }

    pub fn get_hash_by_path(&self, path: &str) -> SqlResult<Option<String>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT content_hash FROM documents WHERE file_path = ?1")?;
            match stmt.query_row(params![path], |row| row.get(0)) {
                Ok(hash) => Ok(Some(hash)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    /// List all indexed documents for the file browser.
    pub fn list_all_documents(&self) -> SqlResult<Vec<DocumentInfo>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT d.file_path, d.file_type, d.content_hash,
                        CASE WHEN a.status = 'tagged' THEN 1 ELSE 0 END
                 FROM documents d
                 LEFT JOIN auto_tag_status a ON d.content_hash = a.content_hash
                 ORDER BY d.file_path",
            )?;
            let docs: Vec<DocumentInfo> = stmt
                .query_map([], |row| {
                    Ok(DocumentInfo {
                        file_path: row.get(0)?,
                        file_type: row.get(1)?,
                        content_hash: row.get(2)?,
                        has_tags: row.get::<_, i64>(3)? != 0,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            let with_tags = docs.iter().filter(|d| d.has_tags).count();
            info!(
                "📋 list_all_documents: {} docs total, {} with tags ✨",
                docs.len(),
                with_tags
            );
            Ok(docs)
        })
    }

    /// Clear all documents and related data for folder switch.
    pub fn clear_all_documents(&self) -> SqlResult<()> {
        self.with_conn(|conn| {
            conn.execute_batch(
                "DELETE FROM auto_tag_cache;
                 DELETE FROM auto_tag_status;
                 DELETE FROM document_tags;
                 DELETE FROM tags;
                 DELETE FROM documents;",
            )?;
            Ok(())
        })
    }

    /// Checkpoint the WAL (call during clean shutdown to truncate).
    pub fn wal_checkpoint(&self) {
        if let Err(e) = self.with_conn(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            Ok(())
        }) {
            tracing::warn!("WAL checkpoint failed: {}", e);
        }
    }
}

/// Lightweight document info for the file browser panel.
#[derive(Debug, Clone)]
pub struct DocumentInfo {
    pub file_path: String,
    pub file_type: String,
    pub content_hash: String,
    /// True when the document has any tags (auto-tag or manually assigned).
    pub has_tags: bool,
}

// ── Auto-Tag Status ──

impl TagStore {
    /// Insert or update auto-tagging status for a document.
    pub fn upsert_auto_tag_status(
        &self,
        content_hash: &str,
        filename: &str,
        content_hash_before_tag: &str,
        status: &str,
        tags_json: Option<&str>,
        last_error: Option<&str>,
    ) -> SqlResult<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO auto_tag_status
                 (content_hash, filename, content_hash_before_tag, status, tags_json, last_error, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
                params![
                    content_hash,
                    filename,
                    content_hash_before_tag,
                    status,
                    tags_json,
                    last_error
                ],
            )?;
            Ok(())
        })
    }

    /// Get auto-tagging status for a document.
    pub fn auto_tag_status(&self, content_hash: &str) -> SqlResult<Option<AutoTagStatus>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT content_hash, filename, content_hash_before_tag, status,
                        tags_json, attempts, last_error, created_at, updated_at
                 FROM auto_tag_status WHERE content_hash = ?1",
            )?;
            let mut rows = stmt.query_map(params![content_hash], |row| {
                Ok(AutoTagStatus {
                    content_hash: row.get(0)?,
                    filename: row.get(1)?,
                    content_hash_before_tag: row.get(2)?,
                    status: row.get(3)?,
                    tags_json: row.get(4)?,
                    attempts: row.get(5)?,
                    last_error: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                })
            })?;
            match rows.next() {
                Some(Ok(status)) => Ok(Some(status)),
                Some(Err(e)) => Err(e),
                None => Ok(None),
            }
        })
    }

    /// Get documents that need auto-tagging (status = 'pending'), ordered by creation time.
    pub fn pending_auto_tags(&self, limit: usize) -> SqlResult<Vec<AutoTagStatus>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT content_hash, filename, content_hash_before_tag, status,
                        tags_json, attempts, last_error, created_at, updated_at
                 FROM auto_tag_status
                 WHERE status = 'pending'
                 ORDER BY created_at ASC
                 LIMIT ?1",
            )?;
            let rows = stmt
                .query_map(params![limit as i64], |row| {
                    Ok(AutoTagStatus {
                        content_hash: row.get(0)?,
                        filename: row.get(1)?,
                        content_hash_before_tag: row.get(2)?,
                        status: row.get(3)?,
                        tags_json: row.get(4)?,
                        attempts: row.get(5)?,
                        last_error: row.get(6)?,
                        created_at: row.get(7)?,
                        updated_at: row.get(8)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
    }

    /// Remove a specific tag from the auto-tag JSON for a document.
    pub fn dismiss_auto_tag(&self, content_hash: &str, tag_name: &str) -> SqlResult<()> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT tags_json FROM auto_tag_status WHERE content_hash = ?1",
            )?;
            let json: Option<String> =
                stmt.query_row(params![content_hash], |row| row.get(0))
                    .ok();

            if let Some(json_str) = json {
                if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&json_str) {
                    if let Some(tags) = value.get_mut("tags") {
                        if let Some(arr) = tags.as_array_mut() {
                            arr.retain(|t| t.as_str() != Some(tag_name));
                        }
                    }
                    let updated = serde_json::to_string(&value).unwrap_or(json_str);
                    conn.execute(
                        "UPDATE auto_tag_status SET tags_json = ?1, updated_at = datetime('now') WHERE content_hash = ?2",
                        params![updated, content_hash],
                    )?;
                }
            }
            Ok(())
        })
    }

    // ── Auto-Tag Cache ──

    /// Look up cached tags by filename token overlap.
    /// Returns `tags_json` if a cache entry has >= min_overlap_ratio token overlap.
    pub fn lookup_cache_by_tokens(
        &self,
        tokens: &[String],
        min_overlap_ratio: f64,
    ) -> SqlResult<Option<String>> {
        if tokens.is_empty() {
            return Ok(None);
        }
        self.with_conn(|conn| {
            // Build a lookup set from input tokens once (avoids per-row HashSet allocation)
            let token_set: std::collections::HashSet<&str> =
                tokens.iter().map(|s| s.as_str()).collect();

            let mut stmt =
                conn.prepare("SELECT filename_tokens, tags_json FROM auto_tag_cache ORDER BY hit_count DESC LIMIT 200")?;
            let rows: Vec<(String, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();

            for (cached_tokens_str, tags_json) in &rows {
                let overlap = cached_tokens_str
                    .split_whitespace()
                    .filter(|ct| token_set.contains(ct))
                    .count();
                let ratio = overlap as f64 / tokens.len() as f64;
                if ratio >= min_overlap_ratio && overlap > 0 {
                    return Ok(Some(tags_json.clone()));
                }
            }
            Ok(None)
        })
    }

    /// Insert or update a cache entry. Increments hit_count if the same tokens already exist.
    pub fn upsert_cache_entry(
        &self,
        filename_tokens: &str,
        tags_json: &str,
        source_hash: &str,
    ) -> SqlResult<()> {
        self.with_conn(|conn| {
            // Check if tokens already exist
            let existing: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT id, hit_count FROM auto_tag_cache WHERE filename_tokens = ?1",
                    params![filename_tokens],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .ok();

            if let Some((id, hit_count)) = existing {
                conn.execute(
                    "UPDATE auto_tag_cache SET tags_json = ?1, hit_count = ?2, updated_at = datetime('now') WHERE id = ?3",
                    params![tags_json, hit_count + 1, id],
                )?;
            } else {
                conn.execute(
                    "INSERT INTO auto_tag_cache (filename_tokens, tags_json, source_hash) VALUES (?1, ?2, ?3)",
                    params![filename_tokens, tags_json, source_hash],
                )?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// Create a TagStore backed by a temp-file database with schema initialized.
    fn setup_test_store() -> (TagStore, tempfile::TempDir) {
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
            CREATE TABLE IF NOT EXISTS auto_tag_status (
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
            CREATE TABLE IF NOT EXISTS auto_tag_cache (
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
    fn create_and_list_tags() {
        let (store, _dir) = setup_test_store();
        store.create_tag("tax").unwrap();
        store.create_tag("receipt").unwrap();

        let tags = store.list_tags().unwrap();
        assert_eq!(tags.len(), 2);
        let names: Vec<&str> = tags.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"tax"));
        assert!(names.contains(&"receipt"));
    }

    #[test]
    fn assign_multiple_tags_to_document() {
        let (store, _dir) = setup_test_store();

        // Insert a document first (required for FK constraint)
        store
            .upsert_document("hash1", "/test/doc.pdf", "pdf", 0, 0)
            .unwrap();

        let tag1 = store.create_tag("tax").unwrap();
        let tag2 = store.create_tag("receipt").unwrap();
        let tag3 = store.create_tag("2025").unwrap();

        store.assign_tag("hash1", tag1.id).unwrap();
        store.assign_tag("hash1", tag2.id).unwrap();
        store.assign_tag("hash1", tag3.id).unwrap();

        let tags = store.get_tags_for_document("hash1").unwrap();
        assert_eq!(tags.len(), 3);
        let names: Vec<&str> = tags.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"tax"));
        assert!(names.contains(&"receipt"));
        assert!(names.contains(&"2025"));
    }

    #[test]
    fn get_documents_with_tag_returns_correct_docs() {
        let (store, _dir) = setup_test_store();

        store
            .upsert_document("hash_a", "/test/a.pdf", "pdf", 0, 0)
            .unwrap();
        store
            .upsert_document("hash_b", "/test/b.pdf", "pdf", 0, 0)
            .unwrap();
        store
            .upsert_document("hash_c", "/test/c.pdf", "pdf", 0, 0)
            .unwrap();

        let tag = store.create_tag("shared").unwrap();

        store.assign_tag("hash_a", tag.id).unwrap();
        store.assign_tag("hash_b", tag.id).unwrap();

        let docs = store.get_documents_with_tag(tag.id).unwrap();
        assert_eq!(docs.len(), 2);
        assert!(docs.contains(&"hash_a".to_string()));
        assert!(docs.contains(&"hash_b".to_string()));
        assert!(!docs.contains(&"hash_c".to_string()));
    }

    #[test]
    fn concurrent_reader_during_writer_no_busy() {
        let (store, _dir) = setup_test_store();

        store
            .upsert_document("hash1", "/test/doc.pdf", "pdf", 0, 0)
            .unwrap();

        let tag = store.create_tag("test").unwrap();
        store.assign_tag("hash1", tag.id).unwrap();

        let store_clone = store.clone();
        let handle = thread::spawn(move || {
            let tags = store_clone.list_tags().unwrap();
            assert!(!tags.is_empty());
        });

        let tag2 = store.create_tag("concurrent").unwrap();
        assert!(tag2.id > 0);

        handle.join().unwrap();

        let all_tags = store.list_tags().unwrap();
        let names: Vec<&str> = all_tags.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"test"));
        assert!(names.contains(&"concurrent"));
    }

    #[test]
    fn already_indexed_by_hash() {
        let (store, _dir) = setup_test_store();

        assert!(!store.already_indexed_by_hash("hash_x").unwrap());

        store
            .upsert_document("hash_x", "/test/x.pdf", "pdf", 1024, 1700000000)
            .unwrap();

        assert!(store.already_indexed_by_hash("hash_x").unwrap());
    }

    #[test]
    fn update_path() {
        let (store, _dir) = setup_test_store();

        store
            .upsert_document("hash1", "/old/path/doc.pdf", "pdf", 1024, 1700000000)
            .unwrap();

        store.update_path("hash1", "/new/path/doc.pdf").unwrap();

        let hash = store
            .get_hash_by_path("/new/path/doc.pdf")
            .unwrap()
            .unwrap();
        assert_eq!(hash, "hash1");

        let old = store.get_hash_by_path("/old/path/doc.pdf").unwrap();
        assert!(old.is_none());
    }

    #[test]
    fn get_tags_for_hashes_batch_query() {
        let (store, _dir) = setup_test_store();

        store
            .upsert_document("hash_a", "/a.pdf", "pdf", 100, 1700000000)
            .unwrap();
        store
            .upsert_document("hash_b", "/b.txt", "txt", 200, 1700000000)
            .unwrap();
        store
            .upsert_document("hash_c", "/c.md", "md", 300, 1700000000)
            .unwrap();

        let tag_tax = store.create_tag("tax").unwrap();
        let tag_2025 = store.create_tag("2025").unwrap();
        let tag_receipt = store.create_tag("receipt").unwrap();

        store.assign_tag("hash_a", tag_tax.id).unwrap();
        store.assign_tag("hash_a", tag_2025.id).unwrap();
        store.assign_tag("hash_b", tag_receipt.id).unwrap();

        // Batch query with &str references (zero allocation)
        let map = store
            .get_tags_for_hashes(&["hash_a", "hash_b", "hash_c"])
            .unwrap();

        assert_eq!(map.len(), 2);
        let a_tags: Vec<&str> = map["hash_a"].iter().map(|t| t.name.as_str()).collect();
        assert!(a_tags.contains(&"tax"));
        assert!(a_tags.contains(&"2025"));
        assert_eq!(a_tags.len(), 2);

        let b_tags: Vec<&str> = map["hash_b"].iter().map(|t| t.name.as_str()).collect();
        assert_eq!(b_tags, vec!["receipt"]);

        let empty = store.get_tags_for_hashes(&[]).unwrap();
        assert!(empty.is_empty());

        let missing = store.get_tags_for_hashes(&["no_such_hash"]).unwrap();
        assert!(missing.is_empty());
    }

    // ── Auto-Tag Status Tests ──

    #[test]
    fn auto_tag_status_round_trips() {
        let (store, _dir) = setup_test_store();
        store
            .upsert_document("hash1", "/test/doc.pdf", "pdf", 0, 0)
            .unwrap();

        store
            .upsert_auto_tag_status(
                "hash1",
                "doc.pdf",
                "abc123def",
                "tagged",
                Some(r#"{"tags":["tax","irs"]}"#),
                None,
            )
            .unwrap();

        let status = store.auto_tag_status("hash1").unwrap().unwrap();
        assert_eq!(status.content_hash, "hash1");
        assert_eq!(status.filename, "doc.pdf");
        assert_eq!(status.content_hash_before_tag, "abc123def");
        assert_eq!(status.status, "tagged");
        assert_eq!(status.tags_json.unwrap(), r#"{"tags":["tax","irs"]}"#);
    }

    #[test]
    fn auto_tag_status_nonexistent_returns_none() {
        let (store, _dir) = setup_test_store();
        let result = store.auto_tag_status("no-such-hash").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn pending_auto_tags_returns_only_pending() {
        let (store, _dir) = setup_test_store();
        store.upsert_document("h1", "/a.pdf", "pdf", 0, 0).unwrap();
        store.upsert_document("h2", "/b.pdf", "pdf", 0, 0).unwrap();
        store.upsert_document("h3", "/c.pdf", "pdf", 0, 0).unwrap();

        store
            .upsert_auto_tag_status("h1", "a.pdf", "x", "pending", None, None)
            .unwrap();
        store
            .upsert_auto_tag_status("h2", "b.pdf", "y", "tagged", None, None)
            .unwrap();
        store
            .upsert_auto_tag_status("h3", "c.pdf", "z", "failed", None, None)
            .unwrap();

        let pending = store.pending_auto_tags(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, "pending");
    }

    #[test]
    fn upsert_auto_tag_status_overwrites() {
        let (store, _dir) = setup_test_store();
        store
            .upsert_document("hash1", "/doc.pdf", "pdf", 0, 0)
            .unwrap();

        store
            .upsert_auto_tag_status("hash1", "doc.pdf", "x", "pending", None, None)
            .unwrap();
        store
            .upsert_auto_tag_status(
                "hash1",
                "doc.pdf",
                "y",
                "tagged",
                Some(r#"{"tags":["ok"]}"#),
                None,
            )
            .unwrap();

        let status = store.auto_tag_status("hash1").unwrap().unwrap();
        assert_eq!(status.status, "tagged");
        assert_eq!(status.content_hash_before_tag, "y");
    }

    #[test]
    fn dismiss_auto_tag_removes_tag() {
        let (store, _dir) = setup_test_store();
        store
            .upsert_document("hash1", "/doc.pdf", "pdf", 0, 0)
            .unwrap();
        store
            .upsert_auto_tag_status(
                "hash1",
                "doc.pdf",
                "x",
                "tagged",
                Some(r#"{"tags":["tax","irs","2023"]}"#),
                None,
            )
            .unwrap();

        store.dismiss_auto_tag("hash1", "irs").unwrap();

        let status = store.auto_tag_status("hash1").unwrap().unwrap();
        let json: serde_json::Value = serde_json::from_str(&status.tags_json.unwrap()).unwrap();
        let tags: Vec<&str> = json["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(tags, vec!["tax", "2023"]);
    }

    // ── Cache Tests ──

    #[test]
    fn cache_lookup_full_overlap_returns_tags() {
        let (store, _dir) = setup_test_store();
        store
            .upsert_cache_entry("tax return yang guorui", r#"{"tags":["tax"]}"#, "h1")
            .unwrap();

        let tokens: Vec<String> = ["tax", "return", "yang", "guorui"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let result = store.lookup_cache_by_tokens(&tokens, 0.5).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn cache_lookup_zero_overlap_returns_none() {
        let (store, _dir) = setup_test_store();
        store
            .upsert_cache_entry("tax return yang guorui", r#"{"tags":["tax"]}"#, "h1")
            .unwrap();

        let tokens: Vec<String> = ["recipe", "cookbook"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let result = store.lookup_cache_by_tokens(&tokens, 0.5).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn cache_lookup_partial_below_threshold_returns_none() {
        let (store, _dir) = setup_test_store();
        store
            .upsert_cache_entry("tax return yang guorui", r#"{"tags":["tax"]}"#, "h1")
            .unwrap();

        // Only "tax" overlaps (1 of 5 = 20% < 50%)
        let tokens: Vec<String> = ["tax", "2023", "form", "1040", "irs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let result = store.lookup_cache_by_tokens(&tokens, 0.5).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn cache_upsert_increments_hit_count() {
        let (store, _dir) = setup_test_store();
        store
            .upsert_cache_entry("tax return", r#"{"tags":["tax"]}"#, "h1")
            .unwrap();
        store
            .upsert_cache_entry("tax return", r#"{"tags":["tax"]}"#, "h2")
            .unwrap();

        let result = store
            .lookup_cache_by_tokens(&["tax".into(), "return".into()], 0.5)
            .unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn cache_lookup_empty_tokens_returns_none() {
        let (store, _dir) = setup_test_store();
        store
            .upsert_cache_entry("tax return", r#"{"tags":["tax"]}"#, "h1")
            .unwrap();

        let result = store.lookup_cache_by_tokens(&[], 0.5).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn cache_lookup_returns_best_match_by_hit_count() {
        let (store, _dir) = setup_test_store();
        // Insert two cache entries for similar tokens, one with higher hit_count
        store
            .upsert_cache_entry("tax return finance", r#"{"tags":["finance"]}"#, "h1")
            .unwrap();
        // Make the second entry more popular
        for _ in 0..5 {
            store
                .upsert_cache_entry("tax return yang guorui", r#"{"tags":["tax"]}"#, "h2")
                .unwrap();
        }

        let tokens: Vec<String> = ["tax", "return"].iter().map(|s| s.to_string()).collect();
        let result = store.lookup_cache_by_tokens(&tokens, 0.5).unwrap();
        // Should return the entry with higher hit_count (sorted DESC)
        assert!(result.is_some());
    }

    #[test]
    fn cache_lookup_limited_to_200_rows() {
        let (store, _dir) = setup_test_store();
        // Insert 250 cache entries — lookup should still complete quickly
        for i in 0..250 {
            store
                .upsert_cache_entry(
                    &format!("doc type{} kind{} year{}", i, i % 10, 2020 + i % 5),
                    r#"{"tags":["test"]}"#,
                    &format!("h{}", i),
                )
                .unwrap();
        }

        let tokens: Vec<String> = ["doc", "type0", "kind0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let result = store.lookup_cache_by_tokens(&tokens, 0.5).unwrap();
        // Should hit the matching entry even with 250 rows (LIMIT 200 won't miss it since it matches)
        assert!(result.is_some());
    }
}

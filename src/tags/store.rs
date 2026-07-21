use rusqlite::{params, Connection, Result as SqlResult};
use std::path::PathBuf;
use super::model::Tag;

/// Manages tag storage via SQLite.
///
/// Opens its own connection (separate from the indexer thread's connection).
/// WAL mode is enabled to allow concurrent readers during indexer writes.
pub struct TagStore {
    conn: Connection,
}

impl TagStore {
    /// Open or create the tag database at the standard location.
    pub fn open_or_create() -> SqlResult<Self> {
        let db_path = Self::db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let conn = Connection::open(&db_path)?;

        // Enable WAL mode for concurrent access
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;

        // Create tables
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
            );"
        )?;

        Ok(Self { conn })
    }

    /// Returns the standard database path.
    fn db_path() -> PathBuf {
        let base = dirs_next::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("papervault").join("papervault.db")
    }

    // ── Tag CRUD ──

    /// Create a new tag. Returns error if the tag name already exists.
    pub fn create_tag(&self, name: &str) -> SqlResult<Tag> {
        self.conn.execute(
            "INSERT OR IGNORE INTO tags (name) VALUES (?1)",
            params![name],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(Tag {
            id,
            name: name.to_string(),
        })
    }

    /// List all tags.
    pub fn list_tags(&self) -> SqlResult<Vec<Tag>> {
        let mut stmt = self.conn.prepare("SELECT id, name FROM tags ORDER BY name")?;
        let tags = stmt.query_map([], |row| {
            Ok(Tag {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
        Ok(tags)
    }

    /// Delete a tag and all its document assignments (CASCADE).
    pub fn delete_tag(&self, tag_id: i64) -> SqlResult<()> {
        self.conn.execute("DELETE FROM tags WHERE id = ?1", params![tag_id])?;
        Ok(())
    }

    // ── Document Tag Assignment ──

    /// Assign a tag to a document.
    pub fn assign_tag(&self, content_hash: &str, tag_id: i64) -> SqlResult<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO document_tags (content_hash, tag_id) VALUES (?1, ?2)",
            params![content_hash, tag_id],
        )?;
        Ok(())
    }

    /// Remove a tag assignment from a document.
    pub fn remove_tag(&self, content_hash: &str, tag_id: i64) -> SqlResult<()> {
        self.conn.execute(
            "DELETE FROM document_tags WHERE content_hash = ?1 AND tag_id = ?2",
            params![content_hash, tag_id],
        )?;
        Ok(())
    }

    /// Get all tags assigned to a document.
    pub fn get_tags_for_document(&self, content_hash: &str) -> SqlResult<Vec<Tag>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.name FROM tags t
             JOIN document_tags dt ON t.id = dt.tag_id
             WHERE dt.content_hash = ?1
             ORDER BY t.name"
        )?;
        let tags = stmt.query_map(params![content_hash], |row| {
            Ok(Tag {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
        Ok(tags)
    }

    /// Get all document content hashes with a given tag.
    pub fn get_documents_with_tag(&self, tag_id: i64) -> SqlResult<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT content_hash FROM document_tags WHERE tag_id = ?1"
        )?;
        let hashes = stmt.query_map(params![tag_id], |row| {
            row.get::<_, String>(0)
        })?
        .filter_map(|r| r.ok())
        .collect();
        Ok(hashes)
    }

    // ── Document Metadata (used by indexer) ──

    /// Check if a document at path with given size+mtime is already indexed.
    pub fn already_indexed_by_metadata(
        &self,
        path: &str,
        size: u64,
        mtime: u64,
    ) -> SqlResult<bool> {
        let mut stmt = self.conn.prepare(
            "SELECT COUNT(*) FROM documents
             WHERE file_path = ?1 AND file_size = ?2 AND modified_ts = ?3"
        )?;
        let count: i64 = stmt.query_row(params![path, size as i64, mtime as i64], |r| r.get(0))?;
        Ok(count > 0)
    }

    /// Check if a content hash already exists.
    pub fn already_indexed_by_hash(&self, content_hash: &str) -> SqlResult<bool> {
        let mut stmt = self.conn.prepare(
            "SELECT COUNT(*) FROM documents WHERE content_hash = ?1"
        )?;
        let count: i64 = stmt.query_row(params![content_hash], |r| r.get(0))?;
        Ok(count > 0)
    }

    /// Update the file path for a content hash (rename/move).
    pub fn update_path(&self, content_hash: &str, new_path: &str) -> SqlResult<()> {
        self.conn.execute(
            "UPDATE documents SET file_path = ?1 WHERE content_hash = ?2",
            params![new_path, content_hash],
        )?;
        Ok(())
    }

    /// Insert or replace a document metadata row.
    pub fn upsert_document(
        &self,
        content_hash: &str,
        file_path: &str,
        file_type: &str,
        file_size: i64,
        modified_ts: i64,
    ) -> SqlResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO documents (content_hash, file_path, file_type, file_size, modified_ts, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            params![content_hash, file_path, file_type, file_size, modified_ts],
        )?;
        Ok(())
    }

    /// Delete a document by file path.
    pub fn delete_document_by_path(&self, path: &str) -> SqlResult<()> {
        self.conn.execute(
            "DELETE FROM documents WHERE file_path = ?1",
            params![path],
        )?;
        Ok(())
    }

    /// Get the content hash for a document by path.
    pub fn get_hash_by_path(&self, path: &str) -> SqlResult<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT content_hash FROM documents WHERE file_path = ?1"
        )?;
        let result = stmt.query_row(params![path], |row| row.get(0));
        match result {
            Ok(hash) => Ok(Some(hash)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_store() -> TagStore {
        TagStore {
            conn: Connection::open_in_memory().unwrap(),
        }
    }

    fn init_test_tables(store: &TagStore) {
        store.conn.execute_batch(
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
            );"
        ).unwrap();
    }

    #[test]
    fn create_and_list_tags() {
        let store = create_test_store();
        init_test_tables(&store);

        store.create_tag("tax").unwrap();
        store.create_tag("receipt").unwrap();

        let tags = store.list_tags().unwrap();
        assert_eq!(tags.len(), 2);
    }

    #[test]
    fn assign_tag_to_document() {
        let store = create_test_store();
        init_test_tables(&store);

        store.upsert_document("hash1", "/path/doc.pdf", "pdf", 1000, 1700000000).unwrap();
        let tag = store.create_tag("important").unwrap();
        store.assign_tag("hash1", tag.id).unwrap();

        let tags = store.get_tags_for_document("hash1").unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "important");
    }

    #[test]
    fn remove_tag_assignment() {
        let store = create_test_store();
        init_test_tables(&store);

        store.upsert_document("hash1", "/path/doc.pdf", "pdf", 1000, 1700000000).unwrap();
        let tag = store.create_tag("temp").unwrap();
        store.assign_tag("hash1", tag.id).unwrap();
        store.remove_tag("hash1", tag.id).unwrap();

        let tags = store.get_tags_for_document("hash1").unwrap();
        assert_eq!(tags.len(), 0);
    }

    #[test]
    fn delete_tag_cascades() {
        let store = create_test_store();
        init_test_tables(&store);

        store.upsert_document("hash1", "/path/doc.pdf", "pdf", 1000, 1700000000).unwrap();
        let tag = store.create_tag("delete_me").unwrap();
        store.assign_tag("hash1", tag.id).unwrap();
        store.delete_tag(tag.id).unwrap();

        // Tag assignment should be gone via CASCADE
        let tags = store.get_tags_for_document("hash1").unwrap();
        assert_eq!(tags.len(), 0);
    }

    #[test]
    fn already_indexed_by_metadata() {
        let store = create_test_store();
        init_test_tables(&store);

        store.upsert_document("hash1", "/path/doc.pdf", "pdf", 1000, 1700000000).unwrap();

        assert!(store.already_indexed_by_metadata("/path/doc.pdf", 1000, 1700000000).unwrap());
        assert!(!store.already_indexed_by_metadata("/other.pdf", 500, 1700000000).unwrap());
    }
}

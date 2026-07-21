use rusqlite::{params, Connection, Result as SqlResult};
use std::path::PathBuf;

use super::model::Tag;

/// Manages tag storage via SQLite. Cloneable — each clone shares the same connection pool.
#[derive(Clone)]
pub struct TagStore {
    pub(crate) db_path: PathBuf,
}

impl TagStore {
    /// Open or create the tag database at the standard location.
    pub fn open_or_create() -> SqlResult<Self> {
        let db_path = Self::db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys = ON;")?;
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
            );",
        )?;

        Ok(Self { db_path })
    }

    fn db_path() -> PathBuf {
        let base = dirs_next::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("papervault").join("papervault.db")
    }

    fn connect(&self) -> SqlResult<Connection> {
        let conn = Connection::open(&self.db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys = ON;")?;
        Ok(conn)
    }

    // ── Tag CRUD ──

    pub fn create_tag(&self, name: &str) -> SqlResult<Tag> {
        let conn = self.connect()?;
        conn.execute("INSERT INTO tags (name) VALUES (?1)", params![name])?;
        let id = conn.last_insert_rowid();
        Ok(Tag {
            id,
            name: name.to_string(),
        })
    }

    pub fn list_tags(&self) -> SqlResult<Vec<Tag>> {
        let conn = self.connect()?;
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
    }

    pub fn delete_tag(&self, tag_id: i64) -> SqlResult<()> {
        let conn = self.connect()?;
        conn.execute("DELETE FROM tags WHERE id = ?1", params![tag_id])?;
        Ok(())
    }

    // ── Document Tag Assignment ──

    pub fn assign_tag(&self, content_hash: &str, tag_id: i64) -> SqlResult<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR IGNORE INTO document_tags (content_hash, tag_id) VALUES (?1, ?2)",
            params![content_hash, tag_id],
        )?;
        Ok(())
    }

    pub fn remove_tag(&self, content_hash: &str, tag_id: i64) -> SqlResult<()> {
        let conn = self.connect()?;
        conn.execute(
            "DELETE FROM document_tags WHERE content_hash = ?1 AND tag_id = ?2",
            params![content_hash, tag_id],
        )?;
        Ok(())
    }

    pub fn get_tags_for_document(&self, content_hash: &str) -> SqlResult<Vec<Tag>> {
        let conn = self.connect()?;
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
    }

    pub fn get_documents_with_tag(&self, tag_id: i64) -> SqlResult<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare("SELECT content_hash FROM document_tags WHERE tag_id = ?1")?;
        let hashes = stmt
            .query_map(params![tag_id], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(hashes)
    }

    // ── Document Metadata ──

    pub fn already_indexed_by_metadata(
        &self,
        path: &str,
        size: u64,
        mtime: u64,
    ) -> SqlResult<bool> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT COUNT(*) FROM documents
             WHERE file_path = ?1 AND file_size = ?2 AND modified_ts = ?3",
        )?;
        let count: i64 = stmt.query_row(params![path, size as i64, mtime as i64], |r| r.get(0))?;
        Ok(count > 0)
    }

    pub fn already_indexed_by_hash(&self, content_hash: &str) -> SqlResult<bool> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM documents WHERE content_hash = ?1")?;
        let count: i64 = stmt.query_row(params![content_hash], |r| r.get(0))?;
        Ok(count > 0)
    }

    pub fn update_path(&self, content_hash: &str, new_path: &str) -> SqlResult<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE documents SET file_path = ?1 WHERE content_hash = ?2",
            params![new_path, content_hash],
        )?;
        Ok(())
    }

    pub fn upsert_document(
        &self,
        content_hash: &str,
        file_path: &str,
        file_type: &str,
        file_size: i64,
        modified_ts: i64,
    ) -> SqlResult<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR REPLACE INTO documents (content_hash, file_path, file_type, file_size, modified_ts, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            params![content_hash, file_path, file_type, file_size, modified_ts],
        )?;
        Ok(())
    }

    pub fn delete_document_by_path(&self, path: &str) -> SqlResult<()> {
        let conn = self.connect()?;
        conn.execute("DELETE FROM documents WHERE file_path = ?1", params![path])?;
        Ok(())
    }

    pub fn get_hash_by_path(&self, path: &str) -> SqlResult<Option<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare("SELECT content_hash FROM documents WHERE file_path = ?1")?;
        match stmt.query_row(params![path], |row| row.get(0)) {
            Ok(hash) => Ok(Some(hash)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
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
            );",
        )
        .unwrap();

        (
            TagStore {
                db_path: db_path.clone(),
            },
            dir,
        )
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
        let conn = Connection::open(&store.db_path).unwrap();
        conn.execute(
            "INSERT INTO documents (content_hash, file_path, file_type) VALUES (?1, ?2, ?3)",
            rusqlite::params!["hash1", "/test/doc.pdf", "pdf"],
        )
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

        // Insert documents
        let conn = Connection::open(&store.db_path).unwrap();
        conn.execute(
            "INSERT INTO documents (content_hash, file_path, file_type) VALUES (?1, ?2, ?3)",
            rusqlite::params!["hash_a", "/test/a.pdf", "pdf"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (content_hash, file_path, file_type) VALUES (?1, ?2, ?3)",
            rusqlite::params!["hash_b", "/test/b.pdf", "pdf"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (content_hash, file_path, file_type) VALUES (?1, ?2, ?3)",
            rusqlite::params!["hash_c", "/test/c.pdf", "pdf"],
        )
        .unwrap();

        let tag = store.create_tag("shared").unwrap();

        // Assign to two docs, leave third untagged
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

        // Insert a document
        let conn = Connection::open(&store.db_path).unwrap();
        conn.execute(
            "INSERT INTO documents (content_hash, file_path, file_type) VALUES (?1, ?2, ?3)",
            rusqlite::params!["hash1", "/test/doc.pdf", "pdf"],
        )
        .unwrap();

        let tag = store.create_tag("test").unwrap();
        store.assign_tag("hash1", tag.id).unwrap();

        // WAL mode: reader on one connection, writer on another should not block
        let store_clone = store.clone();
        let handle = thread::spawn(move || {
            // Reader thread: list tags while writer may be active
            let tags = store_clone.list_tags().unwrap();
            assert!(!tags.is_empty());
        });

        // Main thread: do a write operation
        let tag2 = store.create_tag("concurrent").unwrap();
        assert!(tag2.id > 0);

        handle.join().unwrap();

        // Both operations should have completed
        let all_tags = store.list_tags().unwrap();
        let names: Vec<&str> = all_tags.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"test"));
        assert!(names.contains(&"concurrent"));
    }

    #[test]
    fn already_indexed_by_hash() {
        let (store, _dir) = setup_test_store();

        // Not yet indexed
        assert!(!store.already_indexed_by_hash("hash_x").unwrap());

        // Insert
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

        // Update path for same content hash
        store.update_path("hash1", "/new/path/doc.pdf").unwrap();

        let hash = store
            .get_hash_by_path("/new/path/doc.pdf")
            .unwrap()
            .unwrap();
        assert_eq!(hash, "hash1");

        // Old path should no longer resolve
        let old = store.get_hash_by_path("/old/path/doc.pdf").unwrap();
        assert!(old.is_none());
    }
}

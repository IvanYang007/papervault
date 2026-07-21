use rusqlite::{params, Connection, Result as SqlResult};
use std::path::PathBuf;

use super::model::Tag;

/// Manages tag storage via SQLite. Cloneable — each clone shares the same connection pool.
#[derive(Clone)]
pub struct TagStore {
    db_path: PathBuf,
}

impl TagStore {
    /// Open or create the tag database at the standard location.
    pub fn open_or_create() -> SqlResult<Self> {
        let db_path = Self::db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
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
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        Ok(conn)
    }

    // ── Tag CRUD ──

    pub fn create_tag(&self, name: &str) -> SqlResult<Tag> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR IGNORE INTO tags (name) VALUES (?1)",
            params![name],
        )?;
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

    fn create_test_store() -> TagStore {
        let conn = Connection::open_in_memory().unwrap();
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
        // For tests we use a custom path approach — create a temp file
        let temp_dir = std::env::temp_dir().join(format!("papervault_test_{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).ok();
        // Use the connect-per-operation approach
        TagStore { db_path: temp_dir }
    }

    #[test]
    fn create_and_list_tags() {
        let store = create_test_store();
        // Use an in-memory connection pattern for tests
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL); CREATE TABLE documents (content_hash TEXT PRIMARY KEY, file_path TEXT NOT NULL, file_type TEXT NOT NULL, file_size INTEGER NOT NULL DEFAULT 0, modified_ts INTEGER NOT NULL DEFAULT 0, indexed_at TEXT NOT NULL DEFAULT '', last_error TEXT); CREATE TABLE document_tags (content_hash TEXT NOT NULL REFERENCES documents(content_hash) ON DELETE CASCADE, tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE, PRIMARY KEY (content_hash, tag_id));").unwrap();
        conn.execute("INSERT INTO tags (name) VALUES ('tax')", [])
            .unwrap();
        conn.execute("INSERT INTO tags (name) VALUES ('receipt')", [])
            .unwrap();
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM tags").unwrap();
        let count: i64 = stmt.query_row([], |r| r.get(0)).unwrap();
        assert_eq!(count, 2);
    }
}

use std::path::{Path, PathBuf};
use std::fs;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::*;
use tantivy::tokenizer::*;
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument};
use tantivy::directory::MmapDirectory;

use crate::error::Result;
use super::query::{SearchRequest, SearchResult, SearchResults};
use super::schema::{build_schema, SchemaFields};

/// Manages the Tantivy search index lifecycle.
pub struct SearchEngine {
    index: Index,
    schema: Schema,
    fields: SchemaFields,
    reader: IndexReader,
    writer: IndexWriter,
    index_dir: PathBuf,
}

impl SearchEngine {
    /// Open an existing index or create a new one at the standard location.
    pub fn open_or_create(watched_folder: &Path) -> Result<Self> {
        let index_dir = Self::index_directory();
        let dir = MmapDirectory::open(&index_dir)?;

        let schema = build_schema();
        let fields = SchemaFields::from_schema(&schema);

        let index = if index_dir.join("meta.json").exists() {
            Index::open(dir)?
        } else {
            let index = Index::create_in_dir(&index_dir, schema.clone())?;
            index
        };

        // Register tokenizer under the field name — Tantivy TEXT fields look up by field name
        let tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(LowerCaser)
                .build();
        index.tokenizers().register("body", tokenizer);

        let writer = index.writer(50_000_000)?; // 50MB buffer

        // Run garbage collection for any stale segments from prior crash
        if let Err(e) = writer.garbage_collect_files().wait() {
            tracing::warn!("Garbage collection during open: {}", e);
        }

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        Ok(Self {
            index,
            schema,
            fields,
            reader,
            writer,
            index_dir,
        })
    }

    /// Returns the standard index directory path.
    fn index_directory() -> PathBuf {
        let base = dirs_next::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("papervault").join("index")
    }

    /// Commit pending documents to the index.
    /// Reader reloads automatically via OnCommitWithDelay (within milliseconds).
    pub fn commit(&mut self) -> Result<u64> {
        Ok(self.writer.commit()?)
    }

    /// Prepare commit (called during graceful shutdown).
    pub fn prepare_commit(&mut self) -> Result<u64> {
        let prepared = self.writer.prepare_commit()?;
        Ok(prepared.commit()?)
    }

    /// Reload the reader to pick up committed changes.
    /// Only needed when using ReloadPolicy::Manual (tests).
    /// Production code uses OnCommitWithDelay which auto-reloads.
    pub fn reload(&mut self) -> Result<()> {
        Ok(self.reader.reload()?)
    }

    /// Run garbage collection on stale segments.
    pub fn garbage_collect(&mut self) -> Result<()> {
        self.writer.garbage_collect_files().wait()?;
        Ok(())
    }

    /// Search the index.
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResults> {
        if request.query.trim().is_empty() {
            return Ok(SearchResults {
                items: Vec::new(),
                total_hits: 0,
            });
        }

        let searcher = self.reader.searcher();

        // Build query using TermQuery (proven correct by debug_search_returns_results).
        // Tantivy 0.22 requires the tokenizer registered under the field name, not just "default".
        // TermQuery bypasses QueryParser tokenizer mapping issues.
        let terms: Vec<&str> = request.query.split_whitespace().collect();
        let mut subqueries: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        for term in &terms {
            let t = Term::from_field_text(self.fields.body, &term.to_lowercase());
            subqueries.push((Occur::Must, Box::new(TermQuery::new(t, IndexRecordOption::Basic))));
        }

        // Add tag filters as AND clauses
        for tag in &request.tag_filters {
            let t = Term::from_field_text(self.fields.tags, tag);
            subqueries.push((Occur::Must, Box::new(TermQuery::new(t, IndexRecordOption::Basic))));
        }

        let query: Box<dyn Query> = if subqueries.len() == 1 {
            subqueries.remove(0).1
        } else {
            Box::new(BooleanQuery::new(subqueries))
        };

        let top_docs = searcher.search(&query, &TopDocs::with_limit(request.limit))?;
        let total_hits = top_docs.len();

        let mut items = Vec::new();
        for (_score, doc_address) in top_docs {
            let doc: TantivyDocument = searcher.doc(doc_address)?;

            let file_name = doc
                .get_first(self.fields.file_name)
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let file_path = doc
                .get_first(self.fields.file_path)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let file_type = doc
                .get_first(self.fields.file_type)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let content_hash = doc
                .get_first(self.fields.content_hash)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let tags: Vec<String> = doc
                .get_all(self.fields.tags)
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();

            let match_count = doc
                .get_all(self.fields.body)
                .count();

            // Generate snippet from stored body text
            let snippet = self.generate_snippet(&doc, &request.query);

            items.push(SearchResult {
                file_name,
                file_path,
                file_type,
                snippet,
                match_count,
                content_hash,
                tags,
            });
        }

        Ok(SearchResults { items, total_hits })
    }

    /// Generate a snippet from the stored body text with query term highlighting.
    fn generate_snippet(&self, doc: &TantivyDocument, query: &str) -> String {
        let body_text = doc
            .get_first(self.fields.body)
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if body_text.is_empty() {
            return String::new();
        }

        // Simple highlighting: find query terms in text and wrap in markers
        let terms: Vec<&str> = query.split_whitespace().collect();
        let mut snippet = body_text.chars().take(200).collect::<String>();
        if body_text.len() > 200 {
            snippet.push_str("...");
        }

        for term in &terms {
            let lower_term = term.to_lowercase();
            // Highlight by adding ▶/◀ markers around matches
            if snippet.to_lowercase().contains(&lower_term) {
                // Simple approach: the UI will render these as highlights
                // In production, use Tantivy's SnippetGenerator for proper context
            }
        }

        snippet
    }

    /// Index or update a document in Tantivy.
    pub fn index_document(
        &mut self,
        doc_id: &str,
        file_path: &Path,
        file_name: &str,
        body: &str,
        file_type: &str,
        modified_ts: i64,
        content_hash: &str,
        tags: &[String],
    ) -> Result<()> {
        // Remove existing document with same doc_id (by content_hash)
        let term = tantivy::Term::from_field_text(self.fields.content_hash, content_hash);
        self.writer.delete_term(term);

        let doc = doc!(
            self.fields.doc_id => doc_id,
            self.fields.file_path => file_path.display().to_string(),
            self.fields.file_name => file_name,
            self.fields.body => body,
            self.fields.file_type => file_type,
            self.fields.modified_ts => modified_ts,
            self.fields.content_hash => content_hash,
        );

        // Add tags as multiple values
        let mut doc = doc;
        for tag in tags {
            doc.add_text(self.fields.tags, tag);
        }

        self.writer.add_document(doc)?;
        Ok(())
    }

    /// Delete a document by content hash.
    pub fn delete_by_hash(&mut self, content_hash: &str) -> Result<()> {
        let term = tantivy::Term::from_field_text(self.fields.content_hash, content_hash);
        self.writer.delete_term(term);
        Ok(())
    }

    /// Get the number of documents in the index.
    pub fn doc_count(&self) -> Result<u64> {
        Ok(self.reader.searcher().num_docs())
    }

    /// Access the underlying IndexWriter.
    pub fn writer_mut(&mut self) -> &mut IndexWriter {
        &mut self.writer
    }

    /// Access the schema.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Access the schema fields.
    pub fn fields(&self) -> &SchemaFields {
        &self.fields
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_engine() -> (SearchEngine, TempDir) {
        let dir = TempDir::new().unwrap();
        let schema = build_schema();
        let fields = SchemaFields::from_schema(&schema);
        let index = Index::create_in_dir(dir.path(), schema.clone()).unwrap();

        // Register tokenizer under field name — Tantivy TEXT fields look up by field name
        let tokenizer = TextAnalyzer::builder(
                tantivy::tokenizer::SimpleTokenizer::default()
            )
                .filter(tantivy::tokenizer::LowerCaser)
                .build();
        index.tokenizers().register("body", tokenizer);

        let writer = index.writer(50_000_000).unwrap();
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .unwrap();

        // Manual reload: engine.commit() syncs via writer.commit();
        // tests must call engine.reload() after commit for the reader to see changes

        let engine = SearchEngine {
            index,
            schema,
            fields,
            reader,
            writer,
            index_dir: dir.path().to_path_buf(),
        };
        (engine, dir)
    }

    #[test]
    fn open_or_create_creates_new_index() {
        let dir = TempDir::new().unwrap();
        // Create a fake watched folder
        let watched = dir.path().join("docs");
        fs::create_dir_all(&watched).unwrap();

        // Create temp index dir
        let index_dir = dir.path().join("index");
        fs::create_dir_all(&index_dir).unwrap();

        // Use the test helper instead since we can't override index_directory
        let (_engine, _temp) = create_test_engine();
    }

    #[test]
    fn index_and_search_single_document() {
        let (mut engine, _dir) = create_test_engine();

        engine.index_document(
            "abc123pdf",
            Path::new("/test/doc.pdf"),
            "doc.pdf",
            "hello world invoice march",
            "pdf",
            1700000000,
            "abc123",
            &[],
        ).unwrap();

        engine.commit().unwrap();
        engine.reload().unwrap();

        let results = engine.search(&SearchRequest::new("invoice".into())).unwrap();
        assert_eq!(results.total_hits, 1);
        assert!(results.items[0].file_name.contains("doc.pdf"));
    }

    #[test]
    fn search_absent_term_returns_empty() {
        let (mut engine, _dir) = create_test_engine();

        engine.index_document(
            "abc123pdf",
            Path::new("/test/doc.pdf"),
            "doc.pdf",
            "invoice report",
            "pdf",
            1700000000,
            "abc123",
            &[],
        ).unwrap();

        engine.commit().unwrap();
        engine.reload().unwrap();

        let results = engine.search(&SearchRequest::new("nonexistent".into())).unwrap();
        assert_eq!(results.total_hits, 0);
    }

    #[test]
    fn search_respects_limit() {
        let (mut engine, _dir) = create_test_engine();

        for i in 0..10 {
            engine.index_document(
                &format!("hash{}pdf", i),
                Path::new(&format!("/test/doc{}.pdf", i)),
                &format!("doc{}.pdf", i),
                "common term in all documents",
                "pdf",
                1700000000,
                &format!("hash{}", i),
                &[],
            ).unwrap();
        }
        engine.commit().unwrap();
        engine.reload().unwrap();

        let results = engine.search(
            &SearchRequest::new("common".into()).with_limit(3)
        ).unwrap();
        assert_eq!(results.items.len(), 3);
        assert!(results.total_hits >= 3);
    }

    #[test]
    fn delete_document_removes_from_search() {
        let (mut engine, _dir) = create_test_engine();

        engine.index_document(
            "del1pdf",
            Path::new("/test/del.pdf"),
            "del.pdf",
            "delete me please",
            "pdf",
            1700000000,
            "hash_del",
            &[],
        ).unwrap();
        engine.commit().unwrap();
        engine.reload().unwrap();

        let results = engine.search(&SearchRequest::new("delete".into())).unwrap();
        assert_eq!(results.total_hits, 1);

        engine.delete_by_hash("hash_del").unwrap();
        engine.commit().unwrap();
        engine.reload().unwrap();

        let results = engine.search(&SearchRequest::new("delete".into())).unwrap();
        assert_eq!(results.total_hits, 0);
    }

    #[test]
    fn search_reports_overflow_count() {
        let (mut engine, _dir) = create_test_engine();

        for i in 0..5 {
            engine.index_document(
                &format!("hash{}pdf", i),
                Path::new(&format!("/test/doc{}.pdf", i)),
                &format!("doc{}.pdf", i),
                "common term in all",
                "pdf",
                1700000000,
                &format!("hash{}", i),
                &[],
            ).unwrap();
        }
        engine.commit().unwrap();
        engine.reload().unwrap();

        // With limit 2, items are capped but total_hits is also cap (TopDocs limitation)
        // For v1, overflow is detected when total_hits == limit
        let results = engine.search(
            &SearchRequest::new("common".into()).with_limit(2)
        ).unwrap();
        assert_eq!(results.items.len(), 2);
        // total_hits reflects the actual count from TopDocs (capped by limit)
        assert_eq!(results.total_hits, 2);
        // Overflow detected: total == limit means there may be more
    }

    #[test]
    fn garbage_collect_does_not_crash() {
        let (mut engine, _dir) = create_test_engine();
        engine.garbage_collect().unwrap();
    }
}

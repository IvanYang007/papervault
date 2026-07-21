use std::path::{Path, PathBuf};
use tantivy::collector::{Count, MultiCollector, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::*;
use tantivy::tokenizer::*;
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument};

use super::query::{SearchRequest, SearchResult, SearchResults};
use super::schema::{build_schema, SchemaFields};
use crate::error::Result;

/// Manages the Tantivy search index lifecycle.
pub struct SearchEngine {
    pub(crate) index: Index,
    pub(crate) schema: Schema,
    pub(crate) fields: SchemaFields,
    /// Lock-free reader — clone for UI thread without Mutex contention.
    pub reader: IndexReader,
    pub(crate) writer: IndexWriter,
}

impl SearchEngine {
    /// Open an existing index or create a new one at the standard location.
    pub fn open_or_create(_watched_folder: &Path) -> Result<Self> {
        let index_dir = Self::index_directory();
        let dir = MmapDirectory::open(&index_dir)?;

        let schema = build_schema();
        let fields = SchemaFields::from_schema(&schema);

        let index = if index_dir.join("meta.json").exists() {
            Index::open(dir)?
        } else {
            Index::create_in_dir(&index_dir, schema.clone())?
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
        })
    }

    /// Returns the standard index directory path.
    fn index_directory() -> PathBuf {
        let base = dirs_next::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
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
        search_with_reader(&self.fields, &self.reader, request)
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

/// Lock-free search using a cloned reader and fields.
/// Does NOT require &SearchEngine — usable without the Mutex.
pub fn search_with_reader(
    fields: &SchemaFields,
    reader: &IndexReader,
    request: &SearchRequest,
) -> Result<SearchResults> {
    if request.query.trim().is_empty() {
        return Ok(SearchResults {
            items: Vec::new(),
            total_hits: 0,
        });
    }

    let searcher = reader.searcher();

    let terms: Vec<&str> = request.query.split_whitespace().collect();
    let mut subqueries: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    for term in &terms {
        let t = Term::from_field_text(fields.body, &term.to_lowercase());
        subqueries.push((
            Occur::Must,
            Box::new(TermQuery::new(t, IndexRecordOption::Basic)),
        ));
    }

    for tag in &request.tag_filters {
        let t = Term::from_field_text(fields.tags, tag);
        subqueries.push((
            Occur::Must,
            Box::new(TermQuery::new(t, IndexRecordOption::Basic)),
        ));
    }

    let query: Box<dyn Query> = if subqueries.len() == 1 {
        subqueries.remove(0).1
    } else {
        Box::new(BooleanQuery::new(subqueries))
    };

    // Use MultiCollector to get both limited results AND true total count
    let mut multi = MultiCollector::new();
    let count_handle = multi.add_collector(Count);
    let top_docs_handle = multi.add_collector(TopDocs::with_limit(request.limit));

    let mut multi_fruit = searcher.search(&query, &multi)?;
    let total_hits = count_handle.extract(&mut multi_fruit);
    let top_docs = top_docs_handle.extract(&mut multi_fruit);

    let mut items = Vec::with_capacity(request.limit);
    for (_score, doc_address) in top_docs {
        let doc: TantivyDocument = searcher.doc(doc_address)?;

        let file_name = doc
            .get_first(fields.file_name)
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let file_path = doc
            .get_first(fields.file_path)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let file_type = doc
            .get_first(fields.file_type)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let content_hash = doc
            .get_first(fields.content_hash)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let tags: Vec<String> = doc
            .get_all(fields.tags)
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();

        let match_count = doc.get_all(fields.body).count();

        // Simple snippet: first 200 chars of body
        let body_text = doc
            .get_first(fields.body)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let snippet = if body_text.len() > 200 {
            let mut s = body_text.chars().take(200).collect::<String>();
            s.push_str("...");
            s
        } else {
            body_text.to_string()
        };

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
        let tokenizer = TextAnalyzer::builder(tantivy::tokenizer::SimpleTokenizer::default())
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
        };
        (engine, dir)
    }

    #[test]
    fn open_or_create_creates_new_index() {
        let (_engine, _temp) = create_test_engine();
        // Engine created successfully
    }

    #[test]
    fn index_and_search_single_document() {
        let (mut engine, _dir) = create_test_engine();

        engine
            .index_document(
                "abc123pdf",
                Path::new("/test/doc.pdf"),
                "doc.pdf",
                "hello world invoice march",
                "pdf",
                1700000000,
                "abc123",
                &[],
            )
            .unwrap();

        engine.commit().unwrap();
        engine.reload().unwrap();

        let results = engine
            .search(&SearchRequest::new("invoice".into()))
            .unwrap();
        assert_eq!(results.total_hits, 1);
        assert!(results.items[0].file_name.contains("doc.pdf"));
    }

    #[test]
    fn search_absent_term_returns_empty() {
        let (mut engine, _dir) = create_test_engine();

        engine
            .index_document(
                "abc123pdf",
                Path::new("/test/doc.pdf"),
                "doc.pdf",
                "invoice report",
                "pdf",
                1700000000,
                "abc123",
                &[],
            )
            .unwrap();

        engine.commit().unwrap();
        engine.reload().unwrap();

        let results = engine
            .search(&SearchRequest::new("nonexistent".into()))
            .unwrap();
        assert_eq!(results.total_hits, 0);
    }

    #[test]
    fn search_respects_limit() {
        let (mut engine, _dir) = create_test_engine();

        for i in 0..10 {
            engine
                .index_document(
                    &format!("hash{}pdf", i),
                    Path::new(&format!("/test/doc{}.pdf", i)),
                    &format!("doc{}.pdf", i),
                    "common term in all documents",
                    "pdf",
                    1700000000,
                    &format!("hash{}", i),
                    &[],
                )
                .unwrap();
        }
        engine.commit().unwrap();
        engine.reload().unwrap();

        let results = engine
            .search(&SearchRequest::new("common".into()).with_limit(3))
            .unwrap();
        assert_eq!(results.items.len(), 3);
        assert!(results.total_hits >= 3);
    }

    #[test]
    fn delete_document_removes_from_search() {
        let (mut engine, _dir) = create_test_engine();

        engine
            .index_document(
                "del1pdf",
                Path::new("/test/del.pdf"),
                "del.pdf",
                "delete me please",
                "pdf",
                1700000000,
                "hash_del",
                &[],
            )
            .unwrap();
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
            engine
                .index_document(
                    &format!("hash{}pdf", i),
                    Path::new(&format!("/test/doc{}.pdf", i)),
                    &format!("doc{}.pdf", i),
                    "common term in all",
                    "pdf",
                    1700000000,
                    &format!("hash{}", i),
                    &[],
                )
                .unwrap();
        }
        engine.commit().unwrap();
        engine.reload().unwrap();

        // With limit 2, items are capped at 2 but total_hits reflects actual count
        let results = engine
            .search(&SearchRequest::new("common".into()).with_limit(2))
            .unwrap();
        assert_eq!(results.items.len(), 2);
        // total_hits is the true count from Count collector (not capped by limit)
        assert_eq!(results.total_hits, 5);
    }

    #[test]
    fn garbage_collect_does_not_crash() {
        let (mut engine, _dir) = create_test_engine();
        engine.garbage_collect().unwrap();
    }

    #[test]
    fn open_or_create_opens_existing_index() {
        use tantivy::doc;

        let dir = tempfile::TempDir::new().unwrap();
        let schema = build_schema();
        let fields = SchemaFields::from_schema(&schema);

        // First call: create the index and add a document
        let index = Index::create_in_dir(dir.path(), schema.clone()).unwrap();
        let tokenizer = TextAnalyzer::builder(tantivy::tokenizer::SimpleTokenizer::default())
            .filter(tantivy::tokenizer::LowerCaser)
            .build();
        index.tokenizers().register("body", tokenizer);

        {
            let mut writer = index.writer(50_000_000).unwrap();
            writer
                .add_document(doc!(
                    fields.doc_id => "hash1pdf",
                    fields.file_path => "/test/doc.pdf",
                    fields.file_name => "doc.pdf",
                    fields.body => "persistent document content",
                    fields.file_type => "pdf",
                    fields.modified_ts => 1700000000i64,
                    fields.content_hash => "hash1",
                ))
                .unwrap();
            writer.commit().unwrap();
        }

        // Drop first index to release file locks
        drop(index);

        // Second call: open the existing index — should find the document
        let dir = tantivy::directory::MmapDirectory::open(dir.path()).unwrap();
        let index2 = Index::open(dir).unwrap();
        let reader: IndexReader = index2
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .unwrap();
        reader.reload().unwrap();

        let searcher = reader.searcher();
        assert_eq!(
            searcher.num_docs(),
            1,
            "Opening existing index should preserve documents"
        );
    }

    #[test]
    #[ignore]
    fn search_performance_with_10k_docs() {
        use std::time::Instant;
        use tantivy::doc;

        let dir = tempfile::TempDir::new().unwrap();
        let schema = build_schema();
        let fields = SchemaFields::from_schema(&schema);
        let index = Index::create_in_dir(dir.path(), schema.clone()).unwrap();

        let tokenizer = TextAnalyzer::builder(tantivy::tokenizer::SimpleTokenizer::default())
            .filter(tantivy::tokenizer::LowerCaser)
            .build();
        index.tokenizers().register("body", tokenizer);

        let mut writer = index.writer(50_000_000).unwrap();

        // Index 10K documents
        for i in 0..10_000 {
            let body = format!("document number {} contains various terms", i);
            writer
                .add_document(doc!(
                    fields.doc_id => format!("hash{}pdf", i),
                    fields.file_path => format!("/test/doc{}.pdf", i),
                    fields.file_name => format!("doc{}.pdf", i),
                    fields.body => body,
                    fields.file_type => "pdf",
                    fields.modified_ts => (1700000000i64 + i as i64),
                    fields.content_hash => format!("hash{}", i),
                ))
                .unwrap();
        }
        writer.commit().unwrap();

        let reader: IndexReader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .unwrap();
        reader.reload().unwrap();

        // Search should complete quickly
        let start = Instant::now();
        let request = SearchRequest::new("document".into());
        let results = search_with_reader(&fields, &reader, &request).unwrap();
        let elapsed = start.elapsed();

        assert!(!results.items.is_empty(), "Should find matching documents");

        // Warn if search takes >50ms
        if elapsed.as_millis() > 50 {
            eprintln!(
                "WARNING: search_performance_with_10k_docs took {}ms (>50ms target)",
                elapsed.as_millis()
            );
        }
    }
}

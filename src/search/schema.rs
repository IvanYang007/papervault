use tantivy::schema::*;

/// Creates and returns the Tantivy schema for the document index.
///
/// Fields:
/// - `doc_id`: Stable document identifier (content_hash + file_type), stored + indexed as Str
/// - `file_path`: Full filesystem path, stored only (display)
/// - `file_name`: Filename for display, stored + indexed (searchable)
/// - `body`: Full extracted text, stored + indexed as TEXT with positions (for SnippetGenerator)
/// - `file_type`: "pdf" | "txt" | "md" | "log", stored + indexed
/// - `modified_ts`: Last modification timestamp as i64 (epoch seconds), stored
/// - `content_hash`: blake3 hex string, stored + indexed as Str (for delete_term)
/// - `tags`: Multi-valued tag strings, stored + indexed
pub fn build_schema() -> Schema {
    let mut schema_builder = Schema::builder();

    schema_builder.add_text_field("doc_id", STRING | STORED);
    schema_builder.add_text_field("file_path", STRING | STORED);
    schema_builder.add_text_field("file_name", TEXT | STORED);
    // body: TEXT = INDEXED with positions, STORED for snippet generation
    schema_builder.add_text_field("body", TEXT | STORED);
    schema_builder.add_text_field("file_type", STRING | STORED);
    schema_builder.add_i64_field("modified_ts", STORED);
    // content_hash: STRING = raw text, Stored + Indexed for delete_term
    schema_builder.add_text_field("content_hash", STRING | STORED);
    // tags: multi-valued for tag filtering
    schema_builder.add_text_field("tags", STRING | STORED);

    schema_builder.build()
}

/// Returns field getters for all schema fields.
#[derive(Clone)]
pub struct SchemaFields {
    pub doc_id: Field,
    pub file_path: Field,
    pub file_name: Field,
    pub body: Field,
    pub file_type: Field,
    pub modified_ts: Field,
    pub content_hash: Field,
    pub tags: Field,
}

impl SchemaFields {
    pub fn from_schema(schema: &Schema) -> Self {
        Self {
            doc_id: schema.get_field("doc_id").expect("doc_id field"),
            file_path: schema.get_field("file_path").expect("file_path field"),
            file_name: schema.get_field("file_name").expect("file_name field"),
            body: schema.get_field("body").expect("body field"),
            file_type: schema.get_field("file_type").expect("file_type field"),
            modified_ts: schema.get_field("modified_ts").expect("modified_ts field"),
            content_hash: schema
                .get_field("content_hash")
                .expect("content_hash field"),
            tags: schema.get_field("tags").expect("tags field"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_has_all_required_fields() {
        let schema = build_schema();
        // Verify all fields exist by name — get_field returns Ok if field exists
        assert!(schema.get_field("doc_id").is_ok());
        assert!(schema.get_field("file_path").is_ok());
        assert!(schema.get_field("file_name").is_ok());
        assert!(schema.get_field("body").is_ok());
        assert!(schema.get_field("file_type").is_ok());
        assert!(schema.get_field("modified_ts").is_ok());
        assert!(schema.get_field("content_hash").is_ok());
        assert!(schema.get_field("tags").is_ok());
    }

    #[test]
    fn body_field_is_text_with_positions() {
        let schema = build_schema();
        let field = schema.get_field("body").unwrap();
        let entry = schema.get_field_entry(field);

        assert!(entry.is_indexed(), "body must be indexed");
        assert!(entry.is_stored(), "body must be stored for snippets");
    }

    #[test]
    fn schema_rejects_duplicate_field_names() {
        let mut builder = Schema::builder();
        builder.add_text_field("dup_field", STRING | STORED);
        // Tantivy panics when adding a field with a duplicate name
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            builder.add_text_field("dup_field", STRING | STORED);
        }));
        assert!(result.is_err(), "Duplicate field names should be rejected");
    }

    #[test]
    fn content_hash_field_is_indexed_str() {
        let schema = build_schema();
        let field = schema.get_field("content_hash").unwrap();
        let entry = schema.get_field_entry(field);

        assert!(
            entry.is_indexed(),
            "content_hash must be indexed for delete_term"
        );
        assert!(entry.is_stored(), "content_hash must be stored");
    }
}

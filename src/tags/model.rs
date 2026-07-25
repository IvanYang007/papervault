/// A tag with its database identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub id: i64,
    pub name: String,
}

/// Association between a document (by content hash) and a tag.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct DocumentTag {
    pub content_hash: String,
    pub tag_id: i64,
}

/// Auto-tagging status for a single document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoTagStatus {
    pub content_hash: String,
    pub filename: String,
    pub content_hash_before_tag: String,
    pub status: String,
    pub tags_json: Option<String>,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

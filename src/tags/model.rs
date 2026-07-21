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

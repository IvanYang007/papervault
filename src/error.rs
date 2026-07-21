use thiserror::Error;

#[derive(Error, Debug)]
pub enum PapervaultError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Tantivy error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),

    #[error("SQLite error: {0}")]
    Sqlite(String),

    #[error("PDF extraction error: {0}")]
    PdfExtraction(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Watcher error: {0}")]
    Watcher(String),

    #[error("Channel error: {0}")]
    Channel(String),
}

pub type Result<T> = std::result::Result<T, PapervaultError>;

impl From<rusqlite::Error> for PapervaultError {
    fn from(e: rusqlite::Error) -> Self {
        PapervaultError::Sqlite(e.to_string())
    }
}

impl From<tantivy::directory::error::OpenDirectoryError> for PapervaultError {
    fn from(e: tantivy::directory::error::OpenDirectoryError) -> Self {
        PapervaultError::Tantivy(e.into())
    }
}

impl From<tantivy::query::QueryParserError> for PapervaultError {
    fn from(e: tantivy::query::QueryParserError) -> Self {
        PapervaultError::Tantivy(e.into())
    }
}

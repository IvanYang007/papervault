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

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn error_types_implement_std_error() {
        // Compile-time check: PapervaultError satisfies std::error::Error
        fn assert_error<T: Error>() {}
        assert_error::<PapervaultError>();

        // Verify a conversion also produces a valid Error
        let io_err = PapervaultError::from(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file not found",
        ));
        // Call .source() to confirm the trait chain works
        assert!(io_err.source().is_some());
    }

    #[test]
    fn error_display_is_human_readable() {
        let err = PapervaultError::PdfExtraction("corrupt header at offset 42".into());
        let display = format!("{}", err);
        assert!(
            display.contains("corrupt header"),
            "Display should include the inner message, got: {}",
            display
        );
        assert!(
            display.contains("PDF extraction"),
            "Display should name the error variant, got: {}",
            display
        );

        let io_err = PapervaultError::from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied",
        ));
        let display = format!("{}", io_err);
        assert!(
            display.contains("access denied"),
            "Display should include OS error message, got: {}",
            display
        );
        assert!(
            display.contains("I/O"),
            "Display should include error category, got: {}",
            display
        );
    }
}

//! Error types for the data layer.

use std::fmt;

/// Errors that can occur in the data layer.
#[derive(Debug)]
pub enum Error {
    Sqlite(String),
    Json(String),
    Io(String),
    NotFound(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Sqlite(s) => write!(f, "SQLite error: {s}"),
            Error::Json(s) => write!(f, "JSON error: {s}"),
            Error::Io(s) => write!(f, "I/O error: {s}"),
            Error::NotFound(s) => write!(f, "Not found: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Sqlite(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e.to_string())
    }
}

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug)]
pub enum AppError {
    Io(String),
    Db(String),
    Json(String),
    Invalid(String),
    Http(String),
    NotConfigured,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Io(m) => write!(f, "io error: {m}"),
            AppError::Db(m) => write!(f, "database error: {m}"),
            AppError::Json(m) => write!(f, "json error: {m}"),
            AppError::Invalid(m) => write!(f, "invalid input: {m}"),
            AppError::Http(m) => write!(f, "http error: {m}"),
            AppError::NotConfigured => write!(f, "working directory is not configured"),
        }
    }
}

impl std::error::Error for AppError {}

// Serialized for the frontend as { code, message }.
impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let (code, message) = match self {
            AppError::Io(m) => ("io", m.as_str()),
            AppError::Db(m) => ("db", m.as_str()),
            AppError::Json(m) => ("json", m.as_str()),
            AppError::Invalid(m) => ("invalid", m.as_str()),
            AppError::Http(m) => ("http", m.as_str()),
            AppError::NotConfigured => ("not-configured", ""),
        };
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("code", code)?;
        map.serialize_entry("message", message)?;
        map.end()
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Io(e.to_string())
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::Db(e.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::Json(e.to_string())
    }
}

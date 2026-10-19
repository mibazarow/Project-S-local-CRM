use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug)]
pub enum AppError {
    Io(String),
    Db(String),
    Json(String),
    Invalid(String),
    InvalidConfig(String),
    Http(String),
    // Streaming inactivity watchdog fired: the provider sent no bytes/events
    // for the whole timeout. Distinct code so the UI can offer «Повторить».
    LlmTimeout(String),
    NotConfigured,
}

impl AppError {
    // Stable machine code for the frontend (same string the Serialize impl
    // emits as `code`).
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Io(_) => "io",
            AppError::Db(_) => "db",
            AppError::Json(_) => "json",
            AppError::Invalid(_) => "invalid",
            AppError::InvalidConfig(_) => "invalid-config",
            AppError::Http(_) => "http",
            AppError::LlmTimeout(_) => "llm-timeout",
            AppError::NotConfigured => "not-configured",
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Io(m) => write!(f, "io error: {m}"),
            AppError::Db(m) => write!(f, "database error: {m}"),
            AppError::Json(m) => write!(f, "json error: {m}"),
            AppError::Invalid(m) => write!(f, "invalid input: {m}"),
            AppError::InvalidConfig(m) => write!(f, "invalid_config: {m}"),
            AppError::Http(m) => write!(f, "http error: {m}"),
            // The message is already a full user-readable sentence.
            AppError::LlmTimeout(m) => write!(f, "{m}"),
            AppError::NotConfigured => write!(f, "working directory is not configured"),
        }
    }
}

impl std::error::Error for AppError {}

// Serialized for the frontend as { code, message }.
impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let message = match self {
            AppError::Io(m)
            | AppError::Db(m)
            | AppError::Json(m)
            | AppError::Invalid(m)
            | AppError::InvalidConfig(m)
            | AppError::Http(m)
            | AppError::LlmTimeout(m) => m.as_str(),
            AppError::NotConfigured => "",
        };
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("code", self.code())?;
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

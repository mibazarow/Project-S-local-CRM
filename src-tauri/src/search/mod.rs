mod chat;
mod enrich;
mod fts;
mod ranking;
mod service;
mod text;
mod types;
mod vector;

pub use service::{global_search, search_chats, search_notes, search_project};
pub use types::{ChatHit, GlobalHit, SearchHit};
pub(crate) use service::search_with;
pub(crate) use text::{fts_match_query, tokenize};

#[cfg(test)]
mod tests;

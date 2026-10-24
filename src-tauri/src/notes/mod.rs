// Notes domain. Split per R-11: `fs` holds the Notes/ vault file operations
// (listing, reading, writing, create/rename/move/copy/mkdir/import, quick
// capture, vault summary), `notes_chat` holds the notes-chat SQL and the
// notes agent run startup. The public surface is re-exported here so call
// sites keep the `notes::…` paths.
mod fs;
mod notes_chat;

pub use fs::{
    append_inbox, copy, create, delete, ensure, import, list, mkdir, move_entry, read,
    read_binary_data_url, rename, reveal, tree, write,
};
pub(crate) use fs::{
    copy_at, create_at, delete_at, list_at, mkdir_at, move_at, read_at, rename_at, vault_root,
    write_at,
};
// Test convenience re-exports; other domains' tests call them through this
// module (onboarding, path_policy).
#[cfg(test)]
pub(crate) use fs::{existing_for_mutation_at, WELCOME_NOTE};
pub use notes_chat::{chat_history, ensure_notes_chat, run_notes_chat};

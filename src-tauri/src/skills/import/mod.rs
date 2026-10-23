// Skill management (SPEC 6.2): the backend behind the skills screen —
// import (folder/.zip/git), builtin set install and startup sync, user
// skill creation, and the `create_skill` agent tool. lib.rs wraps the
// plain functions as tauri commands.
//
// Layout: `source` (folder/.zip/git import flow + ImportResult), `archive`
// (safe .zip extraction and the system-git clone seam), `fs_util` (shared
// copy/path/discovery/validation helpers), `builtin` (bundled set install
// and startup sync), and `create` (user skill creation + the create_skill
// tool). This module is a thin facade: it wires the submodules together and
// re-exports the public surface so the `skills::import::…` paths stay put.
mod archive;
mod builtin;
mod create;
mod fs_util;
mod source;

pub use builtin::{install_builtin, sync_builtin};
pub use create::{create, CreateSkillTool};
pub use source::{import, ImportResult};

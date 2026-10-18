// Skills subsystem core (SPEC 6.2): discovery/parse of
// Settings/skills/{builtin,user}/<name>/SKILL.md, the on-demand `skill`
// tool, the short name+description list injected into system prompts, and
// the management commands (list, enable/disable, import folder/.zip/git,
// builtin set install, create). The skills UI is the next slice.
// (S3.5) The subsystem is split by concern into submodules: `manifest`
// (SKILL.md frontmatter parsing, discovery, enabled state, the `skill`
// tool), `import` (folder/.zip/git import, builtin set install/sync, user
// skill creation, the `create_skill` tool) and `exec` (the `skill_exec`
// script runner). All public items are re-exported here so the external
// `skills::…` call paths keep working unchanged.
mod exec;
mod import;
mod manifest;

// Re-exported only for the external call sites that use them (lib.rs,
// commands/skills.rs, onboarding, chat/harness/notes prompts, the agent
// tool catalogs); the rest stay reachable through the submodules.
pub use exec::SkillExecTool;
pub use import::{
    create, import, install_builtin, sync_builtin, CreateSkillTool, ImportResult,
};
pub use manifest::{list, set_enabled, short_list, SkillInfo, SkillTool};
// Only the skills submodule tests call these through `skills::…`.
#[cfg(test)]
pub use manifest::{find, load_full};

// Test helpers shared by the submodule test modules.
#[cfg(test)]
pub(crate) mod testutil {
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use std::fs;
    use std::path::{Path, PathBuf};

    pub(crate) fn write_skill(root: &Path, folder: &str, content: &str) {
        let dir = root.join(folder);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    pub(crate) fn scaffold(tag: &str) -> PathBuf {
        let wd = temp_test_dir(tag);
        for dir in paths::scaffold_dirs(&wd) {
            fs::create_dir_all(dir).unwrap();
        }
        wd
    }
}

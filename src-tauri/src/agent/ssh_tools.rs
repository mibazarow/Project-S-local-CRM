// Agent SSH tools via the system ssh/scp/ssh-keygen CLI (no new crates).
// Profiles live in Settings/config/ssh.json AND in the current project's
// datas.md ("## SSH <name>" sections, the user's chosen storage); both
// sources authenticate with keys/ssh-agent and passwords. Password auth
// goes through SSH_ASKPASS: the password lives only in a 0600 file under
// Harness/tmp/ next to a 0700 askpass script, both deleted when the call
// ends — it never enters argv, tool results or errors. ssh_exec is Mutating
// and CheckConditional: a clearly read-only command (safe first token, no
// shell metacharacters) skips confirmation, everything else confirms with
// the destructive-command analysis enriching the confirmation text.
// ssh_upload/ssh_download/ssh_generate_key/ssh_install_key stay always
// confirmed. Local paths stay inside
// the project root (project mode) or resolve like the harness fs tools
// (harness mode); remote paths pass through with newline/NUL rejection.
// Child processes see TMPDIR/TEMP/TMP pointed at Harness/tmp/ (SPEC 5.2).
// Output is secret-masked and capped; home paths are shortened to ~ in
// errors.
//
// The implementation lives in `agent::ssh` (config, command, askpass,
// session, policy, tools); this module is a thin facade that keeps the
// public path `agent::ssh_tools::*` unchanged.
pub use super::ssh::{
    SshCopyTool, SshExecTool, SshGenerateKeyTool, SshInstallKeyTool, SshListProfilesTool,
    SshSaveProfileToDatasTool,
};
pub(crate) use super::ssh::purge_stale_askpass_dirs;

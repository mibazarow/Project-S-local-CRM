// Password auth without ever putting the secret in argv: a private dir under
// Harness/tmp/ holds a 0600 password file and a 0700 askpass script that cats
// it; ssh reads the password from the script (SSH_ASKPASS with
// SSH_ASKPASS_REQUIRE=force, DISPLAY set so non-tty ssh uses askpass). The
// whole dir is removed when the guard drops — on success, failure or error
// alike. Startup hygiene also purges leftovers from crashed runs.
use super::command::Target;
#[cfg(not(windows))]
use super::command::shell_quote;
use crate::error::{AppError, Result};
use crate::paths;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const ASKPASS_DIR_PREFIX: &str = "ssh-askpass-";

pub(super) struct Askpass {
    pub(super) dir: PathBuf,
    pub(super) script: PathBuf,
    pass: PathBuf,
}

impl Askpass {
    pub(super) fn create(wd: &Path, password: &str) -> Result<Askpass> {
        let tmp = paths::harness_tmp_dir(wd);
        fs::create_dir_all(&tmp)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| AppError::Io(format!("clock unavailable: {e}")))?
            .as_nanos();
        let dir = tmp.join(format!("{ASKPASS_DIR_PREFIX}{}-{nonce}", std::process::id()));
        fs::create_dir(&dir)?;
        crate::platform::restrict_permissions(&dir, 0o700)?;
        let pass = dir.join("pass");
        fs::write(&pass, password)?;
        crate::platform::restrict_permissions(&pass, 0o600)?;

        #[cfg(windows)]
        let script = {
            // Windows OpenSSH cannot execute a .sh askpass helper. Reuse the
            // application binary in a tiny askpass mode: main() sees
            // PROJECT_S_ASKPASS_FILE, prints the password and exits before
            // Tauri starts. No second helper binary is needed in the bundle.
            let exe = std::env::current_exe()
                .map_err(|e| AppError::Io(format!("current executable unavailable: {e}")))?;
            PathBuf::from(crate::platform::path_string(&exe))
        };

        #[cfg(not(windows))]
        let script = {
            let script = dir.join("askpass.sh");
            fs::write(
                &script,
                format!("#!/bin/sh\ncat {}\n", shell_quote(&pass.display().to_string())),
            )?;
            crate::platform::restrict_permissions(&script, 0o700)?;
            script
        };

        Ok(Askpass { dir, script, pass })
    }

    pub(super) fn envs(&self) -> Vec<(String, String)> {
        vec![
            (
                "SSH_ASKPASS".to_string(),
                crate::platform::path_string(&self.script),
            ),
            ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
            ("DISPLAY".to_string(), ":0".to_string()),
            (
                "PROJECT_S_ASKPASS_FILE".to_string(),
                crate::platform::path_string(&self.pass),
            ),
        ]
    }
}

impl Drop for Askpass {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// Password auth for one tool call: Some(guard) while the process runs.
pub(super) fn askpass_for(target: &Target, wd: &Path) -> Result<Option<Askpass>> {
    match &target.password {
        Some(pw) => Askpass::create(wd, pw).map(Some),
        None => Ok(None),
    }
}

// Startup hygiene (refactor S1.7): a crash between Askpass::create and the
// guard's Drop leaves the private dir behind under Harness/tmp/. On startup
// remove leftovers that cannot belong to a live run: entries directly under
// the tmp root named exactly ssh-askpass-<pid>-<nonce> whose pid is dead
// (kill -0, the same probe processes::reconcile uses) AND whose mtime is at
// least ASKPASS_STALE_AGE old. The age guard keeps a concurrent run of
// another instance safe even under pid reuse: ssh calls are capped at
// 30 minutes, well under an hour. Symlink entries are unlinked, never
// followed, and remove_dir_all unlinks links inside the tree without
// following them, so nothing outside Harness/tmp is ever entered or
// removed. Returns the number of removed entries (best-effort: failures
// just leave the entry for the next startup).
const ASKPASS_STALE_AGE: Duration = Duration::from_secs(3600);

pub(crate) fn purge_stale_askpass_dirs(wd: &Path) -> usize {
    let tmp = paths::harness_tmp_dir(wd);
    let Ok(entries) = fs::read_dir(&tmp) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(rest) = name_str.strip_prefix(ASKPASS_DIR_PREFIX) else {
            continue;
        };
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            // Askpass::create only ever makes real dirs, so a matching
            // symlink is never an active askpass dir: unlink the link.
            if fs::remove_file(&path).is_ok() {
                removed += 1;
            }
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        // ssh-askpass-<pid>-<nonce>: anything else is not ours to touch.
        let Some((pid, nonce)) = rest.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if nonce.is_empty() || pid == 0 || crate::platform::process::kill_alive(&pid.to_string()) {
            continue;
        }
        let stale = meta
            .modified()
            .map(|mtime| {
                SystemTime::now()
                    .duration_since(mtime)
                    .unwrap_or(Duration::ZERO)
                    >= ASKPASS_STALE_AGE
            })
            .unwrap_or(false);
        if stale && fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

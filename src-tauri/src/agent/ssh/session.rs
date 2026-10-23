// SSH session/exec: run `ssh`/`scp`/`ssh-keygen` as child processes with
// piped output, TMPDIR/TEMP/TMP pointed at Harness/tmp/, a hard timeout
// (kill), «Стоп» cancel support (the child is killed as soon as the run's
// cancel flag is set) and masked, capped output.
use crate::agent::result::read_capped;
use crate::error::{AppError, Result};
use crate::journal::mask_secrets;
use crate::paths;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(super) const SSH_OUTPUT_CAP: usize = 20_000;
// Envelope cap leaves room for the JSON wrapper around a full 20k output.
pub(super) const SSH_RESULT_CAP: usize = 21_000;

pub(super) struct SshOutput {
    pub(super) code: i32,
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) truncated: bool,
}

// `ssh`/`scp`/`ssh-keygen` with piped output, TMPDIR/TEMP/TMP pointed at
// Harness/tmp/, a hard timeout (kill), «Стоп» cancel support (the child is
// killed as soon as the run's cancel flag is set) and masked output.
// `extra_env` carries the askpass variables for password auth (never any
// secret in argv).
pub(super) fn run_process(
    program: &str,
    argv: &[String],
    wd: &Path,
    timeout: Duration,
    extra_env: &[(String, String)],
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<SshOutput> {
    let tmp = paths::harness_tmp_dir(wd);
    fs::create_dir_all(&tmp)?;
    let mut command = Command::new(program);
    command
        .args(argv)
        .stdin(Stdio::null())
        .env("TMPDIR", &tmp)
        .env("TEMP", &tmp)
        .env("TMP", &tmp)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = match command.spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(AppError::Io(format!("cannot run {program}: {e}"))),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped(r, SSH_OUTPUT_CAP)));
    let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, SSH_OUTPUT_CAP)));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::Io(format!("{program} was cancelled by user")));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::Io(format!(
                "{program} timed out after {} s and was killed",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (stdout, out_cut) = out_thread.join().unwrap_or_default().unwrap_or_default();
    let (stderr, err_cut) = err_thread.join().unwrap_or_default().unwrap_or_default();
    Ok(SshOutput {
        code: status.code().unwrap_or(-1),
        stdout: mask_secrets(&stdout),
        stderr: mask_secrets(&stderr),
        truncated: out_cut || err_cut,
    })
}

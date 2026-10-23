// Minimal cross-platform seams for the Windows demo build. Business logic
// keeps calling these helpers instead of Unix-only APIs: the home directory
// resolver, the executable check, private-file permissions, revealing a file
// in the file manager, and process control (see `process`). The macOS
// behavior is unchanged; Windows gets the native equivalents (or a no-op
// where the concept does not map, e.g. POSIX permission bits).
//
// Not everything OS-specific belongs here: the agent tools that are a macOS
// product feature by nature (clipboard, OCR, PDF, Spotlight, screenshot,
// open_with, browser/CDP) keep their own platform code, because moving them
// would only add a contract list without isolating shared infrastructure.
pub mod process;

use std::path::{Path, PathBuf};
use std::process::Command;

// Prefer the platform home resolver (dirs knows the Windows known folder);
// USERPROFILE/HOME are only fallbacks for stripped-down environments.
pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}

// Default working dir: Documents/PROJECT-S. dirs::document_dir() resolves to
// ~/Documents on macOS and to the known Documents folder on Windows, so the
// macOS result stays the same.
pub fn default_working_dir() -> PathBuf {
    dirs::document_dir()
        .or_else(|| home_dir().map(|home| home.join("Documents")))
        .unwrap_or_else(|| PathBuf::from("/"))
        .join("PROJECT-S")
}

// Is `path` a file the OS can execute directly? Unix checks the executable
// bit; Windows checks the executable extensions.
#[cfg(unix)]
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(windows)]
pub fn is_executable(path: &Path) -> bool {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.is_file() {
        return false;
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "exe" | "cmd" | "bat" | "com"
    )
}

#[cfg(not(any(unix, windows)))]
pub fn is_executable(path: &Path) -> bool {
    path.is_file()
}

// POSIX mode bits; a no-op on Windows, where ACLs are managed by the OS and
// the demo build has no equivalent of chmod 0600/0700.
pub fn restrict_permissions(_path: &Path, _mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(_path, std::fs::Permissions::from_mode(_mode))
    }
    #[cfg(not(unix))]
    {
        Ok(())
    }
}

// Spawned console children must not pop a console window on Windows.
#[allow(dead_code)] // only called from cfg(windows) code paths
pub fn hide_console(_cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = _cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
}

// Show a file in the OS file manager (Finder/Explorer/Files). The Err string
// is the whole message the caller shows, because the command label is part of
// the platform: "open -R" on macOS, "explorer /select," on Windows.
#[cfg(target_os = "macos")]
pub fn reveal_in_file_manager(path: &Path) -> std::result::Result<(), String> {
    match Command::new("/usr/bin/open").arg("-R").arg(path).status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("open -R exited with {status}")),
        Err(e) => Err(format!("open -R: {e}")),
    }
}

// Explorer has no "reveal" flag: it opens the parent folder with the entry
// selected, so the path must be a real file path with backslashes.
#[cfg(windows)]
pub fn reveal_in_file_manager(path: &Path) -> std::result::Result<(), String> {
    let mut cmd = Command::new("explorer.exe");
    cmd.arg(format!("/select,{}", path.display()));
    hide_console(&mut cmd);
    match cmd.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("explorer /select, exited with {status}")),
        Err(e) => Err(format!("explorer /select,: {e}")),
    }
}

// No reveal-with-selection on Linux file managers; opening the parent folder
// is the closest equivalent.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn reveal_in_file_manager(path: &Path) -> std::result::Result<(), String> {
    let parent = path.parent().unwrap_or(path);
    match Command::new("xdg-open").arg(parent).status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("xdg-open exited with {status}")),
        Err(e) => Err(format!("xdg-open: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The macOS message the files UI has always shown, kept verbatim by the
    // seam: the failing command is named, then the OS error or exit status.
    #[cfg(target_os = "macos")]
    #[test]
    fn reveal_in_file_manager_keeps_the_macos_open_message() {
        let missing = std::env::temp_dir().join("project-s-reveal-missing-entry");
        let err = reveal_in_file_manager(&missing)
            .expect_err("a missing path cannot be revealed");
        assert!(err.starts_with("open -R"), "{err}");
    }
}

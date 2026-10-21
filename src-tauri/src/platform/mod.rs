// Minimal cross-platform seams for the Windows demo build. Business logic
// keeps calling these helpers instead of Unix-only APIs: the home directory
// resolver, the executable check and private-file permissions. The macOS
// behavior is unchanged; Windows gets the native equivalents (or a no-op
// where the concept does not map, e.g. POSIX permission bits).
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

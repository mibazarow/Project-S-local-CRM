// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Windows OpenSSH launches SSH_ASKPASS as a separate process. The app
    // binary doubles as that helper: print the password from the private
    // file and exit before Tauri (and the GUI) starts. On Unix the askpass
    // script is used instead, so this branch is normally never taken.
    if let Some(path) = std::env::var_os("PROJECT_S_ASKPASS_FILE") {
        let password = std::fs::read_to_string(path).unwrap_or_default();
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = out.write_all(password.as_bytes());
        let _ = out.flush();
        std::process::exit(0);
    }
    project_s_lib::run();
}

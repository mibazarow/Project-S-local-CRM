// "Open with" chooser: open documents with a suitable non-stock macOS app.
// Asks macOS LaunchServices which apps can actually open the extension
// through a small embedded Swift helper (NSWorkspace.urlsForApplications),
// so unrelated apps (App Store, Activity Monitor, …) are never offered. The
// stock macOS denylist is then applied (the user does not want .docx opened
// in Pages; stock handlers are only kept when nothing else can open the
// file), the rest is ranked by per-extension preference groups, and results
// are cached per extension for the session. The user's per-extension choice
// is remembered in Settings/config/openWith.json, and the file is launched
// with `open -a <app> <file>` (falling back to the system default
// `open <file>` when nothing applies).
use crate::agent::ocr_tools::find_swift;
use crate::agent::result::read_capped;
use crate::error::{AppError, Result};
use crate::fs_ops;
use crate::paths;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const HANDLER_TIMEOUT: Duration = Duration::from_secs(10);
const HANDLER_STDOUT_CAP: usize = 64_000;

// Stock macOS apps, never offered (lowercase bundle-name stems, exact match).
const STOCK_APPS: &[&str] = &[
    "pages",
    "numbers",
    "keynote",
    "textedit",
    "preview",
    "safari",
    "quicktime player",
    "photos",
    "music",
    "tv",
    "books",
    "maps",
    "mail",
    "calendar",
    "contacts",
    "notes",
    "reminders",
    "facetime",
    "messages",
    "garageband",
    "imovie",
    "image capture",
    "stickies",
    "dictionary",
    "calculator",
];

const DOC_EXTS: &[&str] = &["doc", "docx", "rtf", "odt"];
const SHEET_EXTS: &[&str] = &["xls", "xlsx", "csv", "ods"];
const SLIDE_EXTS: &[&str] = &["ppt", "pptx"];
const PDF_EXTS: &[&str] = &["pdf"];
const TEXT_DOC_EXTS: &[&str] = &["txt", "md", "markdown"];

// Per-extension preferred apps in priority order, matched as case-insensitive
// substrings of the .app bundle name (so "ONLYOFFICE DesktopEditors" matches
// "onlyoffice" and "Adobe Acrobat Reader" matches "adobe acrobat").
const PREFERENCES: &[(&[&str], &[&str])] = &[
    (DOC_EXTS, &["microsoft word", "onlyoffice", "libreoffice"]),
    (SHEET_EXTS, &["microsoft excel", "onlyoffice", "libreoffice"]),
    (SLIDE_EXTS, &[
        "microsoft powerpoint",
        "onlyoffice",
        "libreoffice",
    ]),
    (PDF_EXTS, &["adobe acrobat", "pdf expert", "skim"]),
    (fs_ops::IMAGE_EXTS, &[
        "adobe photoshop",
        "affinity photo",
        "pixelmator",
    ]),
    (TEXT_DOC_EXTS, &["visual studio code", "cursor"]),
];

// Swift helper source, written to the system temp dir on demand. It creates
// an empty probe file with the requested extension, asks LaunchServices
// which apps can open it (NSWorkspace.urlsForApplications, macOS 12+), and
// prints the handlers deduplicated by path as JSON [{"name":…,"path":…}] on
// stdout; errors go to stderr with a non-zero exit.
const HANDLERS_SWIFT: &str = r#"import AppKit
import Foundation

func die(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

guard CommandLine.arguments.count >= 2 else {
    die("usage: open_with_handlers.swift <ext>")
}
let ext = CommandLine.arguments[1].lowercased()
guard !ext.isEmpty, ext.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber) }) else {
    die("bad extension: \(ext)")
}

let probe = URL(fileURLWithPath: NSTemporaryDirectory())
    .appendingPathComponent("project-s-probe-\(UUID().uuidString).\(ext)")
try? Data().write(to: probe)
defer { try? FileManager.default.removeItem(at: probe) }

var seen = Set<String>()
var handlers: [[String: String]] = []
for url in NSWorkspace.shared.urlsForApplications(toOpen: probe) {
    if seen.insert(url.path).inserted {
        handlers.append([
            "name": url.deletingPathExtension().lastPathComponent,
            "path": url.path,
        ])
    }
}
guard let data = try? JSONSerialization.data(withJSONObject: handlers) else {
    die("cannot encode handlers as JSON")
}
FileHandle.standardOutput.write(data)
FileHandle.standardOutput.write(Data([0x0A]))
"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub name: String,
    pub path: String,
    pub recommended: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenResult {
    pub opened: bool,
    pub app: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HandlerJson {
    name: String,
    path: String,
}

// Session cache of the final ranked handler list per extension, so the Swift
// helper runs at most once per extension while the app is up. The map is
// reachable only through this module: `get_or_populate` serves hits and
// populates misses (the lister runs outside the lock, so racing misses may
// each populate and the last insert wins), a lister error propagates without
// being cached, and `invalidate` (tests only) drops one entry.
mod app_cache {
    use super::AppInfo;
    use crate::error::Result;
    use crate::sync_util::lock_recover;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<String, Vec<AppInfo>>>> = OnceLock::new();

    fn cache() -> &'static Mutex<HashMap<String, Vec<AppInfo>>> {
        CACHE.get_or_init(Default::default)
    }

    pub fn get_or_populate(
        ext: &str,
        populate: impl FnOnce(&str) -> Result<Vec<AppInfo>>,
    ) -> Result<Vec<AppInfo>> {
        if let Some(cached) = lock_recover(cache()).get(ext) {
            return Ok(cached.clone());
        }
        let apps = populate(ext)?;
        lock_recover(cache()).insert(ext.to_owned(), apps.clone());
        Ok(apps)
    }

    #[cfg(test)]
    pub fn invalidate(ext: &str) {
        lock_recover(cache()).remove(ext);
    }
}

fn is_stock(name: &str) -> bool {
    let lower = name.to_lowercase();
    STOCK_APPS.iter().any(|stock| *stock == lower)
}

// Extensions are passed to the Swift helper as a single argv token; only
// plain alphanumeric extensions are probed at all.
fn normalize_ext(ext: &str) -> Option<String> {
    let ext = ext.trim().trim_start_matches('.').to_lowercase();
    (!ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(ext)
}

fn parse_handlers(stdout: &str) -> Result<Vec<AppInfo>> {
    let parsed: Vec<HandlerJson> = serde_json::from_str(stdout)?;
    let mut seen = HashSet::new();
    Ok(parsed
        .iter()
        .filter(|h| seen.insert(h.path.as_str()))
        .map(|h| AppInfo {
            name: h.name.clone(),
            path: h.path.clone(),
            recommended: false,
        })
        .collect())
}

// Runs the Swift helper with `swift <script> <ext>` and parses its JSON
// stdout; the pattern (capped pipes, poll timeout) follows ocr_tools.rs.
fn probe_handlers(swift: &Path, ext: &str) -> Result<Vec<AppInfo>> {
    let script = std::env::temp_dir().join("project-s-open-with.swift");
    if fs::read_to_string(&script).unwrap_or_default() != HANDLERS_SWIFT {
        fs::write(&script, HANDLERS_SWIFT)?;
    }
    let mut child = Command::new(swift)
        .arg(&script)
        .arg(ext)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread =
        std::thread::spawn(move || stdout.map(|r| read_capped(r, HANDLER_STDOUT_CAP).0));
    let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, 8_192).0));
    let deadline = Instant::now() + HANDLER_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::Io(format!(
                "handler probe timed out after {} s",
                HANDLER_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out_thread.join().unwrap_or_default().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default().unwrap_or_default();
    if status.code() != Some(0) {
        let detail = if stderr.trim().is_empty() {
            stdout.trim().to_owned()
        } else {
            stderr.trim().to_owned()
        };
        return Err(AppError::Io(format!(
            "handler probe failed (exit {}): {detail}",
            status.code().unwrap_or(-1)
        )));
    }
    parse_handlers(&stdout)
}

fn launch_services_handlers(ext: &str) -> Result<Vec<AppInfo>> {
    let Some(swift) = find_swift() else {
        return Err(AppError::Io(
            "swift toolchain not found (checked $PROJECT_S_SWIFT, `which swift`, `xcrun --find swift`); the LaunchServices lookup needs the Xcode Command Line Tools".into(),
        ));
    };
    probe_handlers(&swift, ext)
}

// Applies the stock denylist and the per-extension preference ranking to the
// LaunchServices handlers: preference-group matches first (in group order,
// the first match flagged `recommended`), then the rest alphabetically. When
// every handler is a stock app, the stock handlers are kept rather than
// returning an empty list; an empty handler list stays empty and the UI
// falls back to the system default.
fn rank_handlers(ext: &str, handlers: Vec<AppInfo>) -> Vec<AppInfo> {
    let non_stock: Vec<AppInfo> = handlers
        .iter()
        .filter(|app| !is_stock(&app.name))
        .cloned()
        .collect();
    let mut pool = if non_stock.is_empty() {
        handlers
    } else {
        non_stock
    };
    pool.sort_by_key(|app| app.name.to_lowercase());

    let prefs: &[&str] = PREFERENCES
        .iter()
        .find(|(exts, _)| exts.contains(&ext))
        .map(|(_, prefs)| *prefs)
        .unwrap_or(&[]);
    let mut used = vec![false; pool.len()];
    let mut ranked: Vec<(AppInfo, bool)> = Vec::new();
    let mut recommended = false;
    for pref in prefs {
        for (i, app) in pool.iter().enumerate() {
            if !used[i] && app.name.to_lowercase().contains(pref) {
                used[i] = true;
                let flag = !recommended;
                recommended = true;
                ranked.push((pool[i].clone(), flag));
            }
        }
    }
    for (i, app) in pool.into_iter().enumerate() {
        if !used[i] {
            ranked.push((app, false));
        }
    }
    ranked
        .into_iter()
        .map(|(mut app, flag)| {
            app.recommended = flag;
            app
        })
        .collect()
}

// LaunchServices handlers for the extension, cached per extension for the
// session. A failed lookup is logged and yields an empty list (the dialog
// falls back to "Открыть по умолчанию"); the app folders are never scanned.
pub fn list_apps(ext: &str) -> Vec<AppInfo> {
    let Some(ext) = normalize_ext(ext) else {
        return Vec::new();
    };
    match app_cache::get_or_populate(&ext, |ext| {
        launch_services_handlers(ext).map(|handlers| rank_handlers(ext, handlers))
    }) {
        Ok(apps) => apps,
        Err(e) => {
            eprintln!("[open-with] LaunchServices lookup for \"{ext}\" failed: {e}");
            Vec::new()
        }
    }
}

fn config_file(wd: &Path) -> PathBuf {
    paths::config_dir(wd).join(paths::CONFIG_OPEN_WITH)
}

fn read_map(wd: &Path) -> Result<BTreeMap<String, String>> {
    match fs::read_to_string(config_file(wd)) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

// Validates an absolute-ish path to an existing .app bundle and derives its
// display name from the bundle folder name.
fn validate_app(app_path: &str) -> Result<AppInfo> {
    let path = Path::new(app_path);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".app"));
    let Some(name) = name else {
        return Err(AppError::Invalid(format!("\"{app_path}\" is not a .app bundle")));
    };
    if !path.is_dir() {
        return Err(AppError::Invalid(format!("app \"{app_path}\" not found")));
    }
    Ok(AppInfo {
        name: name.to_owned(),
        path: app_path.to_owned(),
        recommended: false,
    })
}

// The remembered app for the extension; a remembered app that no longer
// exists reads back as None so the chooser flow takes over again.
pub fn get_remembered(wd: &Path, ext: &str) -> Result<Option<AppInfo>> {
    let map = read_map(wd)?;
    let Some(path) = map.get(ext) else {
        return Ok(None);
    };
    Ok(validate_app(path).ok())
}

pub fn remember(wd: &Path, ext: &str, app_path: &str) -> Result<()> {
    let ext = ext.to_lowercase();
    if ext.is_empty() {
        return Err(AppError::Invalid("extension is empty".into()));
    }
    let app = validate_app(app_path)?;
    let mut map = read_map(wd)?;
    map.insert(ext, app.path);
    let file = config_file(wd);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&file, serde_json::to_string_pretty(&map)? + "\n")?;
    Ok(())
}

// Which app to launch: explicit appPath (validated) > remembered > the top
// recommended app for the extension; None means "system default".
pub fn resolve_choice(
    wd: &Path,
    ext: &str,
    app_path: Option<&str>,
) -> Result<Option<AppInfo>> {
    if let Some(path) = app_path {
        return Ok(Some(validate_app(path)?));
    }
    if let Some(app) = get_remembered(wd, ext)? {
        return Ok(Some(app));
    }
    Ok(list_apps(ext).into_iter().find(|app| app.recommended))
}

// Builds the launch command: `open -a <app> <file>` or plain `open <file>`.
pub fn open_command(app_path: Option<&Path>, file: &Path) -> Command {
    let mut cmd = Command::new("/usr/bin/open");
    if let Some(app) = app_path {
        cmd.arg("-a").arg(app);
    }
    cmd.arg(file);
    cmd
}

// Runs the open command with a 15 s timeout; `open` normally returns as soon
// as LaunchServices accepts the request, so the timeout is a safety net.
fn run_open(cmd: &mut Command) -> Result<()> {
    let mut child = cmd.spawn()?;
    let deadline = Instant::now() + OPEN_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(AppError::Io(format!("open exited with {status}")))
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::Io(format!(
                "open timed out after {} s",
                OPEN_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// Resolves the target file through the existing scope resolvers: project
// (project-relative, containment-checked), notes (vault-relative), harness
// (absolute path inside Harness/).
fn resolve_file(
    conn: &Connection,
    wd: &Path,
    scope: &str,
    project_id: Option<i64>,
    path: &str,
) -> Result<PathBuf> {
    match scope {
        "project" => {
            let project_id = project_id
                .ok_or_else(|| AppError::Invalid("project scope needs projectId".into()))?;
            fs_ops::resolve_existing(conn, wd, project_id, path)
        }
        "notes" => {
            let root = paths::notes_dir(wd);
            fs::create_dir_all(&root)?;
            fs_ops::contain_existing(&root.canonicalize()?, path, "Notes")
        }
        "harness" => {
            let target = Path::new(path);
            if !target.is_absolute() {
                return Err(AppError::Invalid("harness path must be absolute".into()));
            }
            let canon = target
                .canonicalize()
                .map_err(|_| AppError::Invalid(format!("\"{path}\" not found")))?;
            let root = paths::harness_dir(wd)
                .canonicalize()
                .map_err(|_| AppError::Invalid("Harness folder not found".into()))?;
            if !canon.starts_with(&root) {
                return Err(AppError::Invalid(format!(
                    "\"{path}\" escapes the Harness folder"
                )));
            }
            if !canon.is_file() {
                return Err(AppError::Invalid(format!("\"{path}\" is not a file")));
            }
            Ok(canon)
        }
        other => Err(AppError::Invalid(format!("unknown scope: {other}"))),
    }
}

pub fn open_with(
    conn: &Connection,
    wd: &Path,
    scope: &str,
    project_id: Option<i64>,
    path: &str,
    app_path: Option<&str>,
) -> Result<OpenResult> {
    let file = resolve_file(conn, wd, scope, project_id, path)?;
    let ext = file
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let chosen = resolve_choice(wd, &ext, app_path)?;
    let mut cmd = open_command(chosen.as_ref().map(|app| Path::new(&app.path)), &file);
    run_open(&mut cmd)?;
    Ok(OpenResult {
        opened: true,
        app: chosen.map(|app| app.name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use std::ffi::OsStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn make_app(dir: &Path, name: &str) -> PathBuf {
        let app = dir.join(format!("{name}.app"));
        fs::create_dir_all(app.join("Contents")).unwrap();
        app
    }

    fn handler(name: &str) -> AppInfo {
        AppInfo {
            name: name.to_owned(),
            path: format!("/Applications/{name}.app"),
            recommended: false,
        }
    }

    fn names(apps: &[AppInfo]) -> Vec<String> {
        apps.iter().map(|a| a.name.clone()).collect()
    }

    #[test]
    fn parses_canned_handler_json_and_dedupes_by_path() {
        let json = r#"[
            {"name":"Microsoft Word","path":"/Applications/Microsoft Word.app"},
            {"name":"Microsoft Word","path":"/Applications/Microsoft Word.app"},
            {"name":"LibreOffice","path":"/Applications/LibreOffice.app"}
        ]"#;
        let handlers = parse_handlers(json).unwrap();
        assert_eq!(names(&handlers), ["Microsoft Word", "LibreOffice"]);
        assert!(handlers.iter().all(|a| !a.recommended));
        assert!(matches!(parse_handlers("not json"), Err(AppError::Json(_))));
        assert!(matches!(parse_handlers("{}"), Err(AppError::Json(_))));
    }

    #[test]
    fn stock_handlers_are_dropped_when_anything_else_can_open() {
        let ranked = rank_handlers(
            "docx",
            vec![handler("Pages"), handler("Microsoft Word"), handler("LibreOffice")],
        );
        assert_eq!(names(&ranked), ["Microsoft Word", "LibreOffice"]);
        assert!(ranked[0].recommended);
        assert!(!ranked[1].recommended);
    }

    #[test]
    fn ranking_prefers_word_over_libreoffice_and_marks_recommended() {
        let ranked = rank_handlers(
            "docx",
            vec![handler("LibreOffice"), handler("Microsoft Word"), handler("GIMP")],
        );
        assert_eq!(names(&ranked), ["Microsoft Word", "LibreOffice", "GIMP"]);
        assert!(ranked[0].recommended);
        assert!(!ranked[1].recommended);
        assert!(!ranked[2].recommended);
        // the spreadsheet group ranks Excel first, Word is not preferred there
        let ranked = rank_handlers(
            "xlsx",
            vec![
                handler("LibreOffice"),
                handler("Microsoft Word"),
                handler("GIMP"),
                handler("Microsoft Excel"),
            ],
        );
        assert_eq!(
            names(&ranked),
            ["Microsoft Excel", "LibreOffice", "GIMP", "Microsoft Word"]
        );
    }

    #[test]
    fn stock_only_handlers_are_kept_and_empty_handlers_stay_empty() {
        // every handler is stock: keep them instead of an empty list
        let ranked = rank_handlers("pdf", vec![handler("Preview")]);
        assert_eq!(names(&ranked), ["Preview"]);
        assert!(rank_handlers("docx", Vec::new()).is_empty());
    }

    #[test]
    fn junk_extensions_are_refused_without_running_swift() {
        for bad in ["", "  ", "../x", "d o c", "docx/"] {
            assert!(normalize_ext(bad).is_none(), "accepted {bad:?}");
            assert!(list_apps(bad).is_empty(), "offered apps for {bad:?}");
        }
        assert_eq!(normalize_ext(" .DOCX "), Some("docx".to_owned()));
    }

    #[test]
    fn probe_with_missing_swift_binary_fails() {
        let err = probe_handlers(Path::new("/no/such/swift"), "docx").unwrap_err();
        assert!(matches!(err, AppError::Io(_)), "{err}");
    }

    #[test]
    fn list_apps_serves_cached_entries_per_extension() {
        let ext = "owcachedext";
        let sentinel = vec![handler("Sentinel Handler")];
        app_cache::get_or_populate(ext, |_| Ok(sentinel.clone())).unwrap();
        // served from the cache: a miss would run Swift and never produce the
        // sentinel app name
        assert_eq!(list_apps(ext), sentinel);
        assert_eq!(list_apps(ext), sentinel);
    }

    #[test]
    fn fake_lister_miss_populates_once_and_hit_skips_the_lister() {
        let calls = AtomicUsize::new(0);
        let first = app_cache::get_or_populate("owfakemiss", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![handler("Fake One")])
        })
        .unwrap();
        assert_eq!(names(&first), ["Fake One"]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let second = app_cache::get_or_populate("owfakemiss", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![handler("Fake Two")])
        })
        .unwrap();
        assert_eq!(second, first);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fake_lister_cache_keys_are_independent() {
        let calls_a = AtomicUsize::new(0);
        let calls_b = AtomicUsize::new(0);
        let a = app_cache::get_or_populate("owfakekeya", |_| {
            calls_a.fetch_add(1, Ordering::SeqCst);
            Ok(vec![handler("Fake A")])
        })
        .unwrap();
        let b = app_cache::get_or_populate("owfakekeyb", |_| {
            calls_b.fetch_add(1, Ordering::SeqCst);
            Ok(vec![handler("Fake B")])
        })
        .unwrap();
        assert_eq!(names(&a), ["Fake A"]);
        assert_eq!(names(&b), ["Fake B"]);
        let again = app_cache::get_or_populate("owfakekeya", |_| Ok(vec![handler("Fake A2")])).unwrap();
        assert_eq!(again, a);
        assert_eq!(calls_a.load(Ordering::SeqCst), 1);
        assert_eq!(calls_b.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fake_lister_invalidate_forces_a_repopulate() {
        let calls = AtomicUsize::new(0);
        app_cache::get_or_populate("owfakeinvalid", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![handler("Fake First")])
        })
        .unwrap();
        let cached = app_cache::get_or_populate("owfakeinvalid", |_| Ok(vec![handler("Fake Never")])).unwrap();
        assert_eq!(names(&cached), ["Fake First"]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        app_cache::invalidate("owfakeinvalid");
        let again = app_cache::get_or_populate("owfakeinvalid", |_| Ok(vec![handler("Fake Second")])).unwrap();
        assert_eq!(names(&again), ["Fake Second"]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fake_lister_error_propagates_and_is_not_cached() {
        let calls = AtomicUsize::new(0);
        let failed = app_cache::get_or_populate("owfakefail", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(AppError::Io("fake lister failure".into()))
        });
        assert!(matches!(failed, Err(AppError::Io(_))));
        let recovered = app_cache::get_or_populate("owfakefail", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![handler("Fake Recovered")])
        })
        .unwrap();
        assert_eq!(names(&recovered), ["Fake Recovered"]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn fake_lister_empty_result_is_cached() {
        let calls = AtomicUsize::new(0);
        let first = app_cache::get_or_populate("owfakeempty", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        })
        .unwrap();
        assert!(first.is_empty());
        let second = app_cache::get_or_populate("owfakeempty", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![handler("Fake Late")])
        })
        .unwrap();
        assert!(second.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fake_lister_concurrent_misses_populate_within_the_locking_window() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let calls = Arc::clone(&calls);
            handles.push(std::thread::spawn(move || {
                app_cache::get_or_populate("owfakeconcurrent", |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    Ok(vec![handler("Fake Concurrent")])
                })
                .unwrap()
            }));
        }
        let results: Vec<Vec<AppInfo>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let populates = calls.load(Ordering::SeqCst);
        assert!(
            (2..=8).contains(&populates),
            "populates {populates} outside the concurrent-miss window"
        );
        for apps in &results {
            assert_eq!(names(apps), ["Fake Concurrent"]);
        }
        let after = app_cache::get_or_populate("owfakeconcurrent", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![handler("Fake Late")])
        })
        .unwrap();
        assert_eq!(names(&after), ["Fake Concurrent"]);
        assert_eq!(calls.load(Ordering::SeqCst), populates);
    }

    #[test]
    fn live_launch_services_handlers_for_common_document_types() {
        if find_swift().is_none() {
            eprintln!("no swift toolchain on this machine, skipping live LaunchServices test");
            return;
        }
        for ext in ["docx", "xlsx", "pdf"] {
            let apps = list_apps(ext);
            let got = names(&apps);
            eprintln!("LaunchServices handlers for {ext}: {got:?}");
            assert!(!apps.is_empty(), "no handlers found for {ext}");
            for unrelated in ["App Store", "Activity Monitor", "System Settings"] {
                assert!(
                    !got.iter().any(|n| n == unrelated),
                    "{unrelated} offered for {ext}: {got:?}"
                );
            }
        }
    }

    #[test]
    fn remember_get_roundtrip_and_validation() {
        let wd = temp_test_dir("ow-cfg");
        let apps = temp_test_dir("ow-cfg-apps");
        let word = make_app(&apps, "Microsoft Word");
        assert!(get_remembered(&wd, "docx").unwrap().is_none());
        remember(&wd, "docx", &word.display().to_string()).unwrap();
        let got = get_remembered(&wd, "docx").unwrap().unwrap();
        assert_eq!(got.name, "Microsoft Word");
        assert_eq!(got.path, word.display().to_string());
        // non-.app and missing paths are refused
        assert!(matches!(
            remember(&wd, "docx", "/Applications/Notes.txt"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            remember(&wd, "docx", "/no/such/Missing.app"),
            Err(AppError::Invalid(_))
        ));
        // a remembered app that no longer exists reads back as None
        fs::remove_dir_all(&word).unwrap();
        assert!(get_remembered(&wd, "docx").unwrap().is_none());
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&apps).unwrap();
    }

    #[test]
    fn open_command_construction() {
        let file = Path::new("/tmp/report.docx");
        let cmd = open_command(Some(Path::new("/Applications/Microsoft Word.app")), file);
        assert_eq!(cmd.get_program(), OsStr::new("/usr/bin/open"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["-a", "/Applications/Microsoft Word.app", "/tmp/report.docx"]
        );
        let cmd = open_command(None, file);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["/tmp/report.docx"]);
    }

    #[test]
    fn resolve_choice_prefers_explicit_then_remembered_then_recommended() {
        let wd = temp_test_dir("ow-choice");
        let apps = temp_test_dir("ow-cfg-apps2");
        let word = make_app(&apps, "Microsoft Word");
        let office = make_app(&apps, "LibreOffice");
        let ext = "owchoice";
        // ranked with the docx preference group, cached under the test ext
        let mut ranked = rank_handlers(
            "docx",
            vec![
                handler("LibreOffice"),
                AppInfo {
                    name: "Microsoft Word".to_owned(),
                    path: word.display().to_string(),
                    recommended: false,
                },
            ],
        );
        for app in ranked.iter_mut() {
            if app.name == "LibreOffice" {
                app.path = office.display().to_string();
            }
        }
        app_cache::get_or_populate(ext, move |_| Ok(ranked)).unwrap();
        // explicit app wins and is validated
        let chosen = resolve_choice(
            &wd,
            ext,
            Some(&office.display().to_string()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(chosen.name, "LibreOffice");
        assert!(matches!(
            resolve_choice(&wd, ext, Some("/etc/hosts")),
            Err(AppError::Invalid(_))
        ));
        // remembered wins over recommended
        remember(&wd, ext, &office.display().to_string()).unwrap();
        let chosen = resolve_choice(&wd, ext, None).unwrap().unwrap();
        assert_eq!(chosen.name, "LibreOffice");
        // no remembered: the top recommended app
        let wd2 = temp_test_dir("ow-choice2");
        let chosen = resolve_choice(&wd2, ext, None).unwrap().unwrap();
        assert_eq!(chosen.name, "Microsoft Word");
        // nothing recommended for the extension: system default (None)
        app_cache::get_or_populate("ownone", |_| Ok(vec![handler("GIMP")])).unwrap();
        assert!(resolve_choice(&wd2, "ownone", None).unwrap().is_none());
        // empty handler list: system default (None)
        app_cache::get_or_populate("owempty", |_| Ok(Vec::new())).unwrap();
        assert!(resolve_choice(&wd2, "owempty", None).unwrap().is_none());
        for dir in [wd, apps, wd2] {
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn scope_resolution_uses_existing_resolvers() {
        let wd = temp_test_dir("ow-scope");
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        fs::write(paths::projects_dir(&wd).join("proj/счёт.docx"), "x").unwrap();
        let resolved = resolve_file(&conn, &wd, "project", Some(dto.id), "счёт.docx").unwrap();
        assert!(resolved.ends_with("счёт.docx"));
        assert!(matches!(
            resolve_file(&conn, &wd, "project", Some(dto.id), "../x"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            resolve_file(&conn, &wd, "project", None, "счёт.docx"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            resolve_file(&conn, &wd, "bogus", None, "x"),
            Err(AppError::Invalid(_))
        ));

        crate::notes::ensure(&wd).unwrap();
        fs::write(paths::notes_dir(&wd).join("нота.docx"), "x").unwrap();
        assert!(resolve_file(&conn, &wd, "notes", None, "нота.docx")
            .unwrap()
            .ends_with("нота.docx"));
        assert!(matches!(
            resolve_file(&conn, &wd, "notes", None, "../x"),
            Err(AppError::Invalid(_))
        ));

        let inbox = paths::harness_inbox_dir(&wd);
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("doc.pdf"), "x").unwrap();
        assert!(resolve_file(
            &conn,
            &wd,
            "harness",
            None,
            &inbox.join("doc.pdf").display().to_string()
        )
        .unwrap()
        .ends_with("doc.pdf"));
        assert!(matches!(
            resolve_file(&conn, &wd, "harness", None, "/etc/hosts"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            resolve_file(&conn, &wd, "harness", None, "doc.pdf"),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }
}

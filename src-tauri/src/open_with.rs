// "Open with" chooser: open documents with a suitable non-stock macOS app.
// Scans the Applications folders for .app bundles, excludes the stock macOS
// denylist (the user does not want .docx opened in Pages), ranks the rest by
// per-extension preference groups, remembers the user's per-extension choice
// in Settings/config/openWith.json, and launches `open -a <app> <file>`
// (falling back to the system default `open <file>` when nothing applies).
use crate::error::{AppError, Result};
use crate::fs_ops;
use crate::paths;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

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

#[derive(Debug, Clone, Serialize)]
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

pub fn default_scan_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Applications"));
    }
    dirs.push(PathBuf::from("/System/Applications"));
    dirs
}

fn is_stock(name: &str) -> bool {
    let lower = name.to_lowercase();
    STOCK_APPS.iter().any(|stock| *stock == lower)
}

// Collects non-stock .app bundles under dir recursively; .app bundles are
// recorded whole and never descended into (their Contents/ subtrees hold
// helper bundles, not user-facing apps).
fn scan_dir(dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    for item in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = item.path();
        if !path.is_dir() {
            continue;
        }
        let name = item.file_name().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(".app") {
            if !stem.is_empty() && !is_stock(stem) {
                out.push((stem.to_owned(), path));
            }
            continue;
        }
        if name == "Contents" || name.starts_with('.') {
            continue;
        }
        scan_dir(&path, out);
    }
}

// Non-stock apps for the extension, ranked: preference-group matches first
// (in group order, the first match flagged `recommended`), then the rest
// alphabetically. Empty when the scan finds no non-stock app at all.
pub fn list_apps(ext: &str, scan_dirs: &[PathBuf]) -> Vec<AppInfo> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for dir in scan_dirs {
        scan_dir(dir, &mut found);
    }
    found.sort_by_key(|entry| entry.0.to_lowercase());
    found.dedup_by(|a, b| a.0.to_lowercase() == b.0.to_lowercase());

    let prefs: &[&str] = PREFERENCES
        .iter()
        .find(|(exts, _)| exts.contains(&ext))
        .map(|(_, prefs)| *prefs)
        .unwrap_or(&[]);
    let mut used = vec![false; found.len()];
    let mut ranked: Vec<(&String, &PathBuf, bool)> = Vec::new();
    let mut recommended = false;
    for pref in prefs {
        for (i, (name, _)) in found.iter().enumerate() {
            if !used[i] && name.to_lowercase().contains(pref) {
                used[i] = true;
                let flag = !recommended;
                recommended = true;
                ranked.push((&found[i].0, &found[i].1, flag));
            }
        }
    }
    for (i, (name, path)) in found.iter().enumerate() {
        if !used[i] {
            ranked.push((name, path, false));
        }
    }
    ranked
        .into_iter()
        .map(|(name, path, recommended)| AppInfo {
            name: name.clone(),
            path: path.display().to_string(),
            recommended,
        })
        .collect()
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
    scan_dirs: &[PathBuf],
    app_path: Option<&str>,
) -> Result<Option<AppInfo>> {
    if let Some(path) = app_path {
        return Ok(Some(validate_app(path)?));
    }
    if let Some(app) = get_remembered(wd, ext)? {
        return Ok(Some(app));
    }
    Ok(list_apps(ext, scan_dirs).into_iter().find(|app| app.recommended))
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
    let chosen = resolve_choice(wd, &ext, &default_scan_dirs(), app_path)?;
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

    fn make_app(dir: &Path, name: &str) -> PathBuf {
        let app = dir.join(format!("{name}.app"));
        fs::create_dir_all(app.join("Contents")).unwrap();
        app
    }

    fn names(apps: &[AppInfo]) -> Vec<String> {
        apps.iter().map(|a| a.name.clone()).collect()
    }

    #[test]
    fn stock_apps_are_excluded_and_nested_bundles_skipped() {
        let dir = temp_test_dir("ow-scan");
        make_app(&dir, "Pages");
        make_app(&dir, "TextEdit");
        make_app(&dir, "Microsoft Word");
        // a helper bundle inside another bundle's Contents is never offered
        fs::create_dir_all(dir.join("Outer.app/Contents/Resources/Helper.app")).unwrap();
        let apps = list_apps("docx", std::slice::from_ref(&dir));
        assert_eq!(names(&apps), ["Microsoft Word", "Outer"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ranking_prefers_word_over_libreoffice_and_marks_recommended() {
        let dir = temp_test_dir("ow-rank");
        make_app(&dir, "LibreOffice");
        make_app(&dir, "Microsoft Word");
        make_app(&dir, "GIMP");
        let apps = list_apps("docx", std::slice::from_ref(&dir));
        assert_eq!(names(&apps), ["Microsoft Word", "LibreOffice", "GIMP"]);
        assert!(apps[0].recommended);
        assert!(!apps[1].recommended);
        assert!(!apps[2].recommended);
        // the spreadsheet group ranks Excel first, Word is not preferred there
        make_app(&dir, "Microsoft Excel");
        let apps = list_apps("xlsx", std::slice::from_ref(&dir));
        assert_eq!(
            names(&apps),
            ["Microsoft Excel", "LibreOffice", "GIMP", "Microsoft Word"]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_scan_yields_empty_list() {
        let dir = temp_test_dir("ow-empty");
        make_app(&dir, "Preview");
        make_app(&dir, "Safari");
        assert!(list_apps("docx", std::slice::from_ref(&dir)).is_empty());
        fs::remove_dir_all(&dir).unwrap();
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
        let apps = temp_test_dir("ow-choice-apps");
        let _word = make_app(&apps, "Microsoft Word");
        let office = make_app(&apps, "LibreOffice");
        // explicit app wins and is validated
        let chosen = resolve_choice(
            &wd,
            "docx",
            std::slice::from_ref(&apps),
            Some(&office.display().to_string()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(chosen.name, "LibreOffice");
        assert!(matches!(
            resolve_choice(&wd, "docx", std::slice::from_ref(&apps), Some("/etc/hosts")),
            Err(AppError::Invalid(_))
        ));
        // remembered wins over recommended
        remember(&wd, "docx", &office.display().to_string()).unwrap();
        let chosen = resolve_choice(&wd, "docx", std::slice::from_ref(&apps), None)
            .unwrap()
            .unwrap();
        assert_eq!(chosen.name, "LibreOffice");
        // no remembered: the top recommended app
        let wd2 = temp_test_dir("ow-choice2");
        let chosen = resolve_choice(&wd2, "docx", std::slice::from_ref(&apps), None)
            .unwrap()
            .unwrap();
        assert_eq!(chosen.name, "Microsoft Word");
        // nothing recommended for the extension: system default (None)
        assert!(resolve_choice(&wd2, "zip", std::slice::from_ref(&apps), None)
            .unwrap()
            .is_none());
        // empty scan: system default (None)
        let empty = temp_test_dir("ow-choice-empty");
        assert!(resolve_choice(&wd2, "docx", std::slice::from_ref(&empty), None)
            .unwrap()
            .is_none());
        for dir in [wd, apps, wd2, empty] {
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

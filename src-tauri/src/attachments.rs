// Chat attachments backend (SPEC 6.1): copy an attached file into the allowed
// scope (project `Входящие/` or `Harness/Входящие/`), journal project copies in
// `INBOX.md`, and turn attachments into model-readable content: documents get
// their extracted text injected into the turn, images are sent as OpenAI
// image_url content parts when the model supports vision (llm::detect_vision)
// and honestly noted otherwise. The source path is never persisted: only the
// copied-in file is, so an attachment from outside the working dir can never
// be referenced without being copied.
use crate::error::{AppError, Result};
use crate::extract;
use crate::fs_ops;
use crate::harness;
use crate::llm;
use crate::paths;
use crate::projects;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

// Per-attachment text cap when injecting into a model turn. The chat run
// path applies it through `message_with_attachments` when rebuilding the
// model-facing transcript (load_history).
pub const ATTACHMENT_MAX_CHARS: usize = 20_000;

// Placeholder note in INBOX.md until the agent/UI supplies a description.
const INBOX_NOTE: &str = "вложение из чата";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentDto {
    pub name: String,
    // Project attachments: project-relative (e.g. "Входящие/счёт.pdf").
    // Harness attachments: absolute path under Harness/Входящие/.
    pub saved_path: String,
    pub kind: &'static str,
    pub size: u64,
    pub is_image: bool,
    pub is_text: bool,
}

// The persisted link between a message and one saved attachment copy, and
// the shape the send_* commands accept for already-saved files (the UI calls
// attach_files first, then sends the returned refs). Same savedPath rules as
// AttachmentDto.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRef {
    pub name: String,
    pub saved_path: String,
    pub kind: String,
    pub is_image: bool,
}

// Rebuilds an AttachmentDto from a stored ref for text extraction; size and
// is_text are display-only and not needed by attachment_text.
pub fn ref_to_dto(attachment: &AttachmentRef) -> AttachmentDto {
    AttachmentDto {
        name: attachment.name.clone(),
        saved_path: attachment.saved_path.clone(),
        kind: match attachment.kind.as_str() {
            "image" => "image",
            "document" => "document",
            _ => "other",
        },
        size: 0,
        is_image: attachment.is_image,
        is_text: false,
    }
}

// Links saved attachment copies to a persisted message row.
pub fn save_message_attachments(
    conn: &Connection,
    message_id: i64,
    attachments: &[AttachmentRef],
) -> Result<()> {
    for attachment in attachments {
        conn.execute(
            "INSERT INTO message_attachments (message_id, name, saved_path, kind, is_image, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now','localtime'))",
            params![
                message_id,
                attachment.name,
                attachment.saved_path,
                attachment.kind,
                attachment.is_image as i64
            ],
        )?;
    }
    Ok(())
}

pub fn load_message_attachments(conn: &Connection, message_id: i64) -> Result<Vec<AttachmentRef>> {
    let mut stmt = conn.prepare(
        "SELECT name, saved_path, kind, is_image
         FROM message_attachments WHERE message_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map(params![message_id], |r| {
        Ok(AttachmentRef {
            name: r.get(0)?,
            saved_path: r.get(1)?,
            kind: r.get(2)?,
            is_image: r.get::<_, i64>(3)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// Resolves an attachment savedPath for the UI: absolute paths (harness
// attachments, and pending source paths before saving) are used as-is,
// project-relative ones (project chats) join the project root.
pub fn resolve_attachment_path(
    conn: &Connection,
    wd: &Path,
    project_id: Option<i64>,
    saved_path: &str,
) -> Result<PathBuf> {
    let path = Path::new(saved_path);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let project_id = project_id
        .ok_or_else(|| AppError::Invalid("relative attachment path needs a project id".into()))?;
    Ok(projects::project_root(conn, wd, project_id)?.join(path))
}

// Reads an attachment image as a base64 data URL for chat thumbnails.
pub fn attachment_data_url(
    conn: &Connection,
    wd: &Path,
    project_id: Option<i64>,
    saved_path: &str,
    max_bytes: u64,
) -> Result<String> {
    let path = resolve_attachment_path(conn, wd, project_id, saved_path)?;
    fs_ops::binary_data_url(&path, max_bytes, saved_path)
}

// Opens an attachment (document or image) with the macOS default handler.
pub fn open_attachment(
    app: &tauri::AppHandle,
    conn: &Connection,
    wd: &Path,
    project_id: Option<i64>,
    saved_path: &str,
) -> Result<()> {
    use tauri_plugin_opener::OpenerExt;
    let path = resolve_attachment_path(conn, wd, project_id, saved_path)?;
    app.opener()
        .open_path(path.display().to_string(), None::<&str>)
        .map_err(|e| AppError::Io(e.to_string()))
}

// Copies each source file into the project's `Входящие/` (created if missing),
// journals each copy in the project's `INBOX.md`, and returns one DTO per file.
pub fn attach_project(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    sources: &[String],
) -> Result<Vec<AttachmentDto>> {
    let root = projects::project_root(conn, wd, project_id)?;
    let inbox = root.join(paths::PROJECT_INBOX);
    fs::create_dir_all(&inbox)?;
    let mut out = Vec::with_capacity(sources.len());
    for source in sources {
        let target = copy_into(&inbox, source)?;
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        append_inbox(
            &root.join("INBOX.md"),
            &inbox_line(&name, &format!("проект #{project_id}")),
        )?;
        out.push(dto(&target, Some(&root)));
    }
    Ok(out)
}

// Copies each source file into `Harness/Входящие/` for a harness session and
// returns one DTO per file (absolute savedPath). No project index or INBOX.md:
// a harness session is not bound to a project (SPEC 6.1).
pub fn attach_harness(
    conn: &Connection,
    wd: &Path,
    session_id: i64,
    sources: &[String],
) -> Result<Vec<AttachmentDto>> {
    harness::session_chat_id(conn, session_id)?;
    let inbox = paths::harness_inbox_dir(wd);
    fs::create_dir_all(&inbox)?;
    let mut out = Vec::with_capacity(sources.len());
    for source in sources {
        let target = copy_into(&inbox, source)?;
        out.push(dto(&target, None));
    }
    Ok(out)
}

// Extracts an attachment's text for the model. Images return None (they are
// sent as image content parts by build_user_content instead); unsupported/other
// formats return None. `base` is the project root for project attachments
// (savedPath is project-relative); harness savedPaths are absolute, so `base`
// is ignored there.
pub fn attachment_text(attachment: &AttachmentDto, base: &Path, max_chars: usize) -> Option<String> {
    if attachment.is_image {
        return None;
    }
    let path = resolve_saved_path(attachment, base);
    if !extract::is_supported(&path) {
        return None;
    }
    extract::extract_text(&path, max_chars).ok()
}

// The model-facing user turn: expanded text plus image data URLs to attach as
// OpenAI image_url content parts (empty when vision is off or an image could
// not be read).
#[derive(Debug, Clone)]
pub struct UserTurn {
    pub text: String,
    pub images: Vec<String>,
}

// Builds the model-facing user turn: the user's text followed by one clearly
// delimited block per document attachment (carrying its extracted text).
// Images: with vision supported they become data-URL content parts (no text
// block); otherwise the turn carries an honest Russian note and the image is
// not sent. load_history applies it when rebuilding the transcript for a run.
pub fn build_user_content(
    text: &str,
    attachments: &[AttachmentDto],
    base: &Path,
    vision_supported: bool,
) -> UserTurn {
    let mut out = text.to_string();
    let mut images = Vec::new();
    for attachment in attachments {
        if attachment.is_image {
            if vision_supported {
                let path = resolve_saved_path(attachment, base);
                match llm::image_data_url(&path) {
                    Ok(url) => images.push(url),
                    Err(e) => out.push_str(&format!(
                        "\n\n[Изображение «{}» не отправлено: {}]",
                        attachment.name, e
                    )),
                }
            } else {
                out.push_str(&format!(
                    "\n\n[Изображение «{}» не отправлено: модель не поддерживает vision]",
                    attachment.name
                ));
            }
            continue;
        }
        out.push_str(&format!("\n\n[Вложение: {}]", attachment.name));
        match attachment_text(attachment, base, ATTACHMENT_MAX_CHARS) {
            Some(body) => out.push_str(&format!("\n{body}")),
            None => out.push_str("\n(формат файла не разбирается)"),
        }
    }
    UserTurn {
        text: out,
        images,
    }
}

fn resolve_saved_path(attachment: &AttachmentDto, base: &Path) -> PathBuf {
    let saved = Path::new(&attachment.saved_path);
    if saved.is_absolute() {
        saved.to_path_buf()
    } else {
        base.join(saved)
    }
}

// Copies one absolute source file into `dest_dir`, sanitizing its name and
// suffixing on collision. Directories and missing paths are rejected.
fn copy_into(dest_dir: &Path, source: &str) -> Result<PathBuf> {
    let src = PathBuf::from(source);
    if !src.is_absolute() {
        return Err(AppError::Invalid(format!(
            "source path \"{source}\" must be absolute"
        )));
    }
    let canon = src
        .canonicalize()
        .map_err(|_| AppError::Invalid(format!("source \"{source}\" not found")))?;
    if !canon.is_file() {
        return Err(AppError::Invalid(format!(
            "source \"{source}\" is not a file"
        )));
    }
    let raw = canon
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| AppError::Invalid(format!("source \"{source}\" has no name")))?;
    let name = fs_ops::check_name(&raw)?;
    let target = dest_dir.join(fs_ops::unique_name(dest_dir, &name, false));
    fs::copy(&canon, &target)?;
    Ok(target)
}

fn dto(target: &Path, root: Option<&Path>) -> AttachmentDto {
    let ext = target
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let is_image = fs_ops::IMAGE_EXTS.contains(&ext.as_str());
    let is_text = extract::kind(target) == Some("text");
    let kind = if is_image {
        "image"
    } else if extract::is_supported(target) {
        "document"
    } else {
        "other"
    };
    let saved_path = match root {
        Some(root) => target
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| target.display().to_string()),
        None => target.display().to_string(),
    };
    AttachmentDto {
        name: target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        saved_path,
        kind,
        size: fs::metadata(target).map(|m| m.len()).unwrap_or(0),
        is_image,
        is_text,
    }
}

fn inbox_line(filename: &str, source: &str) -> String {
    let date = chrono::Local::now().format("%Y-%m-%d");
    format!("- {date} · {filename} · {source} · {INBOX_NOTE}")
}

// Append-only journal: a header on first write, then one line per attachment.
// An identical line is never written twice, so re-running the same attach does
// not duplicate the entry.
fn append_inbox(inbox_md: &Path, line: &str) -> Result<()> {
    let existing = match fs::read_to_string(inbox_md) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::from("# Входящие\n\n"),
        Err(e) => return Err(e.into()),
    };
    if existing.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(line);
    updated.push('\n');
    fs::write(inbox_md, updated)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexingConfig;
    use crate::db;
    use crate::embed;
    use crate::index;
    use crate::onboarding::temp_test_dir;
    use crate::state::AppState;
    use rusqlite::params;
    use std::io::Write as _;

    // File-backed working dir + project, mirroring the index tests so the
    // indexing pipeline (which opens its own connection) can see the files.
    fn setup(tag: &str) -> (PathBuf, Connection, i64, AppState) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        let state = AppState::new();
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(db::open(&paths::db_path(&wd)).unwrap());
        (wd, conn, dto.id, state)
    }

    fn root(wd: &Path) -> PathBuf {
        paths::projects_dir(wd).join("proj")
    }

    fn source_dir(tag: &str) -> PathBuf {
        temp_test_dir(tag)
    }

    fn fts_rel_paths(conn: &Connection, project_id: i64, query: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT rel_path FROM files_fts WHERE files_fts MATCH ?1 AND project_id = ?2")
            .unwrap();
        stmt.query_map(params![query, project_id.to_string()], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn docx_bytes(text: &str) -> Vec<u8> {
        let xml = format!(
            "<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"
        );
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(xml.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn attach_txt_to_project_copies_journals_and_is_searchable() {
        let (wd, conn, pid, state) = setup("att-txt");
        let src = source_dir("att-txt-src");
        fs::write(src.join("note.txt"), "Клиент example.ru оплатил счёт").unwrap();

        let items =
            attach_project(&conn, &wd, pid, &[src.join("note.txt").display().to_string()]).unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.name, "note.txt");
        assert_eq!(item.saved_path, "Входящие/note.txt");
        assert_eq!(item.kind, "document");
        assert!(item.is_text && !item.is_image);
        assert!(root(&wd).join("Входящие/note.txt").is_file());

        let inbox = fs::read_to_string(root(&wd).join("INBOX.md")).unwrap();
        assert!(inbox.starts_with("# Входящие"));
        assert!(inbox.contains("note.txt"));
        assert!(inbox.contains("проект #"));

        index::index_project(&state, pid).unwrap();
        assert_eq!(fts_rel_paths(&conn, pid, "оплатил"), ["Входящие/note.txt"]);

        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn name_collision_gets_a_suffix() {
        let (wd, conn, pid, _state) = setup("att-collide");
        let src = source_dir("att-collide-src");
        fs::write(src.join("note.txt"), "one").unwrap();
        let first =
            attach_project(&conn, &wd, pid, &[src.join("note.txt").display().to_string()]).unwrap();
        let second =
            attach_project(&conn, &wd, pid, &[src.join("note.txt").display().to_string()]).unwrap();
        assert_eq!(first[0].saved_path, "Входящие/note.txt");
        assert_eq!(second[0].saved_path, "Входящие/note copy.txt");
        assert!(root(&wd).join("Входящие/note copy.txt").is_file());
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn docx_attachment_extracts_text() {
        let (wd, conn, pid, _state) = setup("att-docx");
        let src = source_dir("att-docx-src");
        fs::write(src.join("счёт.docx"), docx_bytes("Итого к оплате 1000")).unwrap();
        let items =
            attach_project(&conn, &wd, pid, &[src.join("счёт.docx").display().to_string()]).unwrap();
        assert_eq!(items[0].kind, "document");
        assert!(!items[0].is_text);
        let text = attachment_text(&items[0], &root(&wd), 1000).unwrap();
        assert_eq!(text, "Итого к оплате 1000");
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn image_is_saved_flagged_and_has_no_text() {
        let (wd, conn, pid, _state) = setup("att-image");
        let src = source_dir("att-image-src");
        fs::write(src.join("pic.png"), llm::TINY_PNG).unwrap();
        let items =
            attach_project(&conn, &wd, pid, &[src.join("pic.png").display().to_string()]).unwrap();
        assert_eq!(items[0].kind, "image");
        assert!(items[0].is_image);
        assert!(root(&wd).join("Входящие/pic.png").is_file());
        assert!(attachment_text(&items[0], &root(&wd), 1000).is_none());

        let turn = build_user_content("что на картинке?", &items, &root(&wd), false);
        assert!(turn.images.is_empty());
        assert!(turn
            .text
            .contains("[Изображение «pic.png» не отправлено: модель не поддерживает vision]"));
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn harness_attachment_lands_in_harness_inbox_with_absolute_path() {
        let (wd, conn, _pid, _state) = setup("att-harness");
        let session = harness::create_session(&conn, Some("s")).unwrap();
        let src = source_dir("att-harness-src");
        fs::write(src.join("log.txt"), "строчка").unwrap();
        let items =
            attach_harness(&conn, &wd, session.id, &[src.join("log.txt").display().to_string()])
                .unwrap();
        assert_eq!(items.len(), 1);
        let saved = Path::new(&items[0].saved_path);
        assert!(saved.is_absolute());
        assert_eq!(saved, paths::harness_inbox_dir(&wd).join("log.txt"));
        assert!(saved.is_file());
        // No project INBOX.md was written for a harness attach.
        assert!(!root(&wd).join("INBOX.md").exists());
        // Unknown session is rejected.
        assert!(matches!(
            attach_harness(&conn, &wd, 999, &[src.join("log.txt").display().to_string()]),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn inbox_does_not_duplicate_an_identical_line() {
        let dir = temp_test_dir("att-inbox-idem");
        let file = dir.join("INBOX.md");
        let line = inbox_line("note.txt", "проект #1");
        append_inbox(&file, &line).unwrap();
        append_inbox(&file, &line).unwrap();
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(text.matches("note.txt").count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_relative_missing_and_directory_sources() {
        let (wd, conn, pid, _state) = setup("att-badsrc");
        assert!(matches!(
            attach_project(&conn, &wd, pid, &["note.txt".into()]),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            attach_project(&conn, &wd, pid, &["/definitely/missing/file.txt".into()]),
            Err(AppError::Invalid(_))
        ));
        let src = source_dir("att-badsrc-dir");
        fs::create_dir_all(src.join("folder")).unwrap();
        assert!(matches!(
            attach_project(&conn, &wd, pid, &[src.join("folder").display().to_string()]),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn message_injects_document_text_delimited_and_capped() {
        let (wd, conn, pid, _state) = setup("att-msg");
        let src = source_dir("att-msg-src");
        fs::write(src.join("a.md"), "данные проекта").unwrap();
        let items =
            attach_project(&conn, &wd, pid, &[src.join("a.md").display().to_string()]).unwrap();
        let turn = build_user_content("проверь счёт", &items, &root(&wd), false);
        assert!(turn.text.starts_with("проверь счёт"));
        assert!(turn.text.contains("[Вложение: a.md]\nданные проекта"));
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn document_text_is_capped_at_the_attachment_limit() {
        let (wd, conn, pid, _state) = setup("att-msg-cap");
        let src = source_dir("att-msg-cap-src");
        // Body longer than the cap, with a unique tail marker past the cut.
        let body = format!("{}ХВОСТ", "д".repeat(ATTACHMENT_MAX_CHARS + 10));
        fs::write(src.join("big.md"), &body).unwrap();
        let items =
            attach_project(&conn, &wd, pid, &[src.join("big.md").display().to_string()]).unwrap();
        let turn = build_user_content("x", &items, &root(&wd), false);
        assert!(turn.text.contains("[Вложение: big.md]"));
        assert!(!turn.text.contains("ХВОСТ"));
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn build_user_content_sends_image_parts_when_vision_supported() {
        let (wd, conn, pid, _state) = setup("att-turn-vision");
        let src = source_dir("att-turn-vision-src");
        fs::write(src.join("pic.png"), llm::TINY_PNG).unwrap();
        fs::write(src.join("a.md"), "данные проекта").unwrap();
        let items = attach_project(
            &conn,
            &wd,
            pid,
            &[
                src.join("pic.png").display().to_string(),
                src.join("a.md").display().to_string(),
            ],
        )
        .unwrap();

        let turn = build_user_content("что на картинке?", &items, &root(&wd), true);
        // The image becomes a data URL part, not a text note...
        assert_eq!(turn.images.len(), 1);
        assert!(turn.images[0].starts_with("data:image/png;base64,"));
        // ...while the document still contributes its extracted text.
        assert!(turn.text.starts_with("что на картинке?"));
        assert!(turn.text.contains("[Вложение: a.md]\nданные проекта"));
        assert!(!turn.text.contains("не отправлено"));
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn build_user_content_notes_images_but_keeps_documents_when_vision_off() {
        let (wd, conn, pid, _state) = setup("att-turn-novision");
        let src = source_dir("att-turn-novision-src");
        fs::write(src.join("pic.png"), llm::TINY_PNG).unwrap();
        fs::write(src.join("a.md"), "данные проекта").unwrap();
        let items = attach_project(
            &conn,
            &wd,
            pid,
            &[
                src.join("pic.png").display().to_string(),
                src.join("a.md").display().to_string(),
            ],
        )
        .unwrap();

        let turn = build_user_content("проверь", &items, &root(&wd), false);
        assert!(turn.images.is_empty());
        assert!(turn.text.contains("[Вложение: a.md]\nданные проекта"));
        assert!(turn
            .text
            .contains("[Изображение «pic.png» не отправлено: модель не поддерживает vision]"));
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn resolve_attachment_path_handles_absolute_and_project_relative() {
        let (wd, conn, pid, _state) = setup("att-resolve");
        // project_root canonicalizes, so compare against the canonical root
        // (macOS temp dirs live behind a /var -> /private/var symlink).
        let canon_root = root(&wd).canonicalize().unwrap();
        assert_eq!(
            resolve_attachment_path(&conn, &wd, Some(pid), "Входящие/a.txt").unwrap(),
            canon_root.join("Входящие/a.txt")
        );
        assert_eq!(
            resolve_attachment_path(&conn, &wd, None, "/abs/Harness/Входящие/a.txt").unwrap(),
            PathBuf::from("/abs/Harness/Входящие/a.txt")
        );
        // A relative savedPath without a project context cannot be resolved.
        assert!(matches!(
            resolve_attachment_path(&conn, &wd, None, "Входящие/a.txt"),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn noop_embedder_path_is_used_by_index_project() {
        // Sanity: the default config selects the noop embedder, so indexing a
        // freshly attached file stays offline.
        let (wd, _conn, _pid, _state) = setup("att-noop");
        let cfg = IndexingConfig::default();
        assert_eq!(embed::select_embedder(&cfg.embedding, &wd).backend_id(), "noop");
        fs::remove_dir_all(&wd).unwrap();
    }
}

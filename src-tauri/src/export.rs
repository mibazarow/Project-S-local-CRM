use crate::error::Result;
use crate::{paths, projects};
use rusqlite::{params, Connection};
use std::fs;
use std::path::Path;

// Mirrors the tasks table into tasks/<id>-<slug>.md inside each project folder.
// SQLite stays the source of truth; these files are a one-way, idempotent export.
pub fn export_project(conn: &Connection, wd: &Path, project_id: i64) -> Result<()> {
    let root = projects::project_root(conn, wd, project_id)?;
    let project_name: String =
        conn.query_row("SELECT name FROM projects WHERE id = ?1", params![project_id], |r| r.get(0))?;
    let dir = root.join(paths::PROJECT_TASKS);
    fs::create_dir_all(&dir)?;

    let mut expected = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT id, title, status, comment, due_date, remind_at, tags, created_at, closed_at
         FROM tasks WHERE project_id = ?1",
    )?;
    let mut rows = stmt.query(params![project_id])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let title: String = row.get(1)?;
        let file = file_name(id, &title);
        fs::write(dir.join(&file), render(row, &project_name)?)?;
        expected.push(file);
    }

    remove_stale(&dir, &expected)?;
    Ok(())
}

fn file_name(id: i64, title: &str) -> String {
    let slug = slugify(title);
    if slug.is_empty() {
        format!("{id}.md")
    } else {
        format!("{id}-{slug}.md")
    }
}

// ASCII-only slug; non-latin titles produce an empty slug and fall back to <id>.md.
fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            if !out.is_empty() && pending_dash {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    out
}

fn render(row: &rusqlite::Row<'_>, project: &str) -> Result<String> {
    let id: i64 = row.get(0)?;
    let title: String = row.get(1)?;
    let status: String = row.get(2)?;
    let comment: Option<String> = row.get(3)?;
    let due: Option<String> = row.get(4)?;
    let remind: Option<String> = row.get(5)?;
    let tags_json: String = row.get(6)?;
    let created: String = row.get(7)?;
    let closed: Option<String> = row.get(8)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    let mut out = String::from("---\n");
    out.push_str(&format!("id: {id}\n"));
    out.push_str(&format!("title: {}\n", yaml_quote(&title)));
    out.push_str(&format!("status: {status}\n"));
    out.push_str(&format!("project: {}\n", yaml_quote(project)));
    out.push_str(&format!("created: {}\n", yaml_quote(&created)));
    if let Some(d) = &due {
        out.push_str(&format!("due: \"{d}\"\n"));
    }
    if let Some(c) = &closed {
        out.push_str(&format!("closed: {}\n", yaml_quote(c)));
    }
    if let Some(r) = &remind {
        out.push_str(&format!("remind: \"{r}\"\n"));
    }
    let tags_out = tags
        .iter()
        .map(|t| yaml_quote(t))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("tags: [{tags_out}]\n"));
    out.push_str("---\n\n");
    out.push_str(&format!("# {title}\n"));
    if let Some(c) = comment {
        out.push_str(&format!("\n{c}\n"));
    }
    Ok(out)
}

// YAML double-quoted scalar: always safe, escapes the few risky characters.
fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// Only files matching our own "<id>-*.md" / "<id>.md" pattern are ever removed.
fn remove_stale(dir: &Path, expected: &[String]) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if expected.contains(&name) || !is_ours(&name) {
            continue;
        }
        fs::remove_file(dir.join(&name))?;
    }
    Ok(())
}

fn is_ours(name: &str) -> bool {
    let digits = name.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || !name.ends_with(".md") {
        return false;
    }
    let rest = &name[digits..];
    rest == ".md" || (rest.starts_with('-') && rest.len() > 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use std::path::PathBuf;

    fn setup(tag: &str) -> (PathBuf, Connection, i64, PathBuf) {
        let wd = temp_test_dir(tag);
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        let project = projects::create_project(&conn, &wd, "p1").unwrap();
        let tasks_dir = paths::projects_dir(&wd).join("p1").join(paths::PROJECT_TASKS);
        (wd, conn, project.id, tasks_dir)
    }

    fn list_md(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn export_writes_frontmatter_and_body() {
        let (wd, conn, pid, dir) = setup("export-write");
        crate::tasks::task_create(&conn, pid, "Fix Header!", None).unwrap();
        let t = crate::tasks::task_create(&conn, pid, "Правка шапки", None).unwrap();
        crate::tasks::task_update(
            &conn,
            t.id,
            crate::tasks::TaskPatch {
                comment: Some(Some("детали\nвторой абзац".into())),
                due_date: Some(Some("2099-01-02".into())),
                remind_at: Some(Some("2099-01-02T10:30".into())),
                tags: Some(vec!["ui".into(), "срочно".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        export_project(&conn, &wd, pid).unwrap();

        assert_eq!(list_md(&dir), vec!["1-fix-header.md", "2.md"]);
        let one = fs::read_to_string(dir.join("1-fix-header.md")).unwrap();
        assert!(one.starts_with("---\nid: 1\ntitle: \"Fix Header!\"\nstatus: new\nproject: \"p1\"\n"));
        assert!(one.contains("tags: []\n---\n\n# Fix Header!\n"));
        let two = fs::read_to_string(dir.join("2.md")).unwrap();
        assert!(two.contains("due: \"2099-01-02\"\n"));
        assert!(two.contains("remind: \"2099-01-02T10:30\"\n"));
        assert!(two.contains("tags: [\"ui\", \"срочно\"]\n"));
        assert!(two.contains("---\n\n# Правка шапки\n\nдетали\nвторой абзац\n"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn export_is_idempotent_and_updates_in_place() {
        let (wd, conn, pid, dir) = setup("export-idempotent");
        let t = crate::tasks::task_create(&conn, pid, "task", None).unwrap();
        export_project(&conn, &wd, pid).unwrap();
        let first = fs::read_to_string(dir.join("1-task.md")).unwrap();
        export_project(&conn, &wd, pid).unwrap();
        assert_eq!(first, fs::read_to_string(dir.join("1-task.md")).unwrap());

        crate::tasks::task_move(&conn, t.id, "done", 1.0).unwrap();
        export_project(&conn, &wd, pid).unwrap();
        let updated = fs::read_to_string(dir.join("1-task.md")).unwrap();
        assert!(updated.contains("status: done\n"));
        assert!(updated.contains("closed: \""));
        assert_eq!(list_md(&dir), vec!["1-task.md"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn export_removes_stale_task_files_but_keeps_user_files() {
        let (wd, conn, pid, dir) = setup("export-stale");
        let t = crate::tasks::task_create(&conn, pid, "temp task", None).unwrap();
        export_project(&conn, &wd, pid).unwrap();
        assert!(dir.join("1-temp-task.md").is_file());

        for user_file in ["notes.md", "my-1.md", "readme.txt", "1task.md"] {
            fs::write(dir.join(user_file), "keep me").unwrap();
        }

        crate::tasks::task_delete(&conn, t.id).unwrap();
        export_project(&conn, &wd, pid).unwrap();
        assert!(!dir.join("1-temp-task.md").exists());
        for user_file in ["notes.md", "my-1.md", "readme.txt", "1task.md"] {
            assert!(dir.join(user_file).is_file(), "deleted user file {user_file}");
        }
        fs::remove_dir_all(&wd).unwrap();
    }
}

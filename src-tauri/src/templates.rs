// Reusable templates for projects, tasks, notes and documents, stored in
// Settings/config/templates.json next to the other hand-editable configs.
// The file is seeded with defaults on first use; unknown top-level keys
// survive app rewrites (same flatten-extra pattern as mcp.json).
use crate::error::{AppError, Result};
use crate::fs_ops;
use crate::notes;
use crate::paths;
use crate::projects::{self, ProjectDto};
use crate::tasks::{self, ChecklistItem, TaskDto};
use fs_ops::FsEntry;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TemplateFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProjectTemplate {
    pub name: String,
    pub description: Option<String>,
    pub files: Vec<TemplateFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskTemplate {
    pub name: String,
    pub title: String,
    pub priority: Option<String>,
    pub checklist: Option<Vec<ChecklistItem>>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NoteTemplate {
    pub name: String,
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DocumentTemplate {
    pub name: String,
    pub markdown: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TemplatesConfig {
    pub projects: Vec<ProjectTemplate>,
    pub tasks: Vec<TaskTemplate>,
    pub notes: Vec<NoteTemplate>,
    pub documents: Vec<DocumentTemplate>,
    // Unknown top-level keys survive app rewrites (SPEC 3.4 hand-editable configs).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// One template row for the UI pickers: name plus a short one-line description.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateSummary {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplatesSummary {
    pub projects: Vec<TemplateSummary>,
    pub tasks: Vec<TemplateSummary>,
    pub notes: Vec<TemplateSummary>,
    pub documents: Vec<TemplateSummary>,
}

// Result of template_document: the markdown the agent/UI fills in and then
// creates through the create_document tool.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentDto {
    pub name: String,
    pub markdown: String,
}

pub fn config_file(wd: &Path) -> PathBuf {
    paths::config_dir(wd).join(paths::CONFIG_TEMPLATES)
}

pub fn defaults() -> TemplatesConfig {
    TemplatesConfig {
        projects: vec![ProjectTemplate {
            name: "Новый проект".into(),
            description: Some("Пустой проект с файлом datas.md".into()),
            files: vec![TemplateFile {
                path: paths::DATAS_MD.into(),
                content: "# Данные проекта\n\n- Клиент: ИП Петров (example.ru)\n- Договор: № 000/2026\n- Контакты: +7 000 000-00-00\n".into(),
            }],
        }],
        tasks: vec![TaskTemplate {
            name: "Позвонить клиенту".into(),
            title: "Позвонить клиенту".into(),
            priority: Some("high".into()),
            checklist: Some(vec![
                ChecklistItem {
                    text: "Поднять переписку".into(),
                    done: false,
                },
                ChecklistItem {
                    text: "Записать итог звонка в datas.md".into(),
                    done: false,
                },
            ]),
            tags: Some(vec!["клиент".into()]),
        }],
        notes: vec![NoteTemplate {
            name: "Заметка о встрече".into(),
            content: "# Встреча\n\nДата: \nУчастники: \n\n## Обсудили\n\n- \n\n## Договорились\n\n- \n\n## Следующий шаг\n\n- \n".into(),
        }],
        documents: vec![DocumentTemplate {
            name: "Коммерческое предложение".into(),
            markdown: "# Коммерческое предложение\n\nКлиент: ИП Петров (example.ru)\nДата: \n\n## Что предлагаем\n\n- пункт 1\n- пункт 2\n\n## Стоимость\n\n| Услуга | Цена |\n| --- | --- |\n| Работы | 0 ₽ |\n\n## Условия\n\n- Предложение действительно 14 дней.\n".into(),
        }],
        extra: Map::new(),
    }
}

// Reads templates.json, seeding the shipped defaults when the file is missing.
pub fn load(wd: &Path) -> Result<TemplatesConfig> {
    let file = config_file(wd);
    match fs::read_to_string(&file) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = defaults();
            save(wd, &cfg)?;
            Ok(cfg)
        }
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn save(wd: &Path, cfg: &TemplatesConfig) -> Result<()> {
    let file = config_file(wd);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&file, serde_json::to_string_pretty(cfg)? + "\n")?;
    Ok(())
}

fn not_found(kind: &str, name: &str) -> AppError {
    AppError::Invalid(format!("{kind} template \"{name}\" not found"))
}

fn find<'a, T>(list: &'a [T], name: &str, kind: &str, item_name: impl Fn(&T) -> &str) -> Result<&'a T> {
    list.iter()
        .find(|item| item_name(item) == name)
        .ok_or_else(|| not_found(kind, name))
}

// First non-heading line of a template body, shortened for the picker rows.
fn preview(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or_default();
    match line.chars().nth(80) {
        Some(_) => format!("{}…", line.chars().take(80).collect::<String>()),
        None => line.to_owned(),
    }
}

pub fn summary(wd: &Path) -> Result<TemplatesSummary> {
    let cfg = load(wd)?;
    Ok(TemplatesSummary {
        projects: cfg
            .projects
            .iter()
            .map(|t| TemplateSummary {
                name: t.name.clone(),
                description: t
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("{} файл(ов)", t.files.len())),
            })
            .collect(),
        tasks: cfg
            .tasks
            .iter()
            .map(|t| TemplateSummary {
                name: t.name.clone(),
                description: t.title.clone(),
            })
            .collect(),
        notes: cfg
            .notes
            .iter()
            .map(|t| TemplateSummary {
                name: t.name.clone(),
                description: preview(&t.content),
            })
            .collect(),
        documents: cfg
            .documents
            .iter()
            .map(|t| TemplateSummary {
                name: t.name.clone(),
                description: preview(&t.markdown),
            })
            .collect(),
    })
}

// Project template file paths must stay project-relative: absolute paths,
// ".." hops and empty segments are rejected before anything is created.
fn validate_rel(rel: &str) -> Result<()> {
    if rel.trim().is_empty() {
        return Err(AppError::Invalid("template file path is empty".into()));
    }
    fs_ops::join_rel(Path::new("/"), rel)?;
    Ok(())
}

// Creates the project through the normal create_project scaffolding, then
// writes the template files on top. An existing name gets " 2", " 3", ...
// appended so the picker never fails on a clash.
pub fn create_project(
    conn: &Connection,
    wd: &Path,
    template_name: &str,
    project_name: Option<&str>,
) -> Result<ProjectDto> {
    let cfg = load(wd)?;
    let tpl = find(&cfg.projects, template_name, "project", |t| &t.name)?;
    for file in &tpl.files {
        validate_rel(&file.path)?;
    }
    let base = project_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&tpl.name);
    let base = projects::sanitize_project_name(base)?;
    let projects_dir = paths::projects_dir(wd);
    let mut name = base.clone();
    let mut n = 1u32;
    while projects_dir.join(&name).exists() {
        n += 1;
        name = format!("{base} {n}");
        if n > 1000 {
            return Err(AppError::Invalid(format!(
                "too many projects named \"{base}\""
            )));
        }
    }
    let dto = projects::create_project(conn, wd, &name)?;
    let root = projects::project_root(conn, wd, dto.id)?;
    for file in &tpl.files {
        let target = fs_ops::join_rel(&root, &file.path)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, &file.content)?;
    }
    Ok(dto)
}

// Creates the task with the template's title/priority/checklist/tags in the
// 'new' column; tags go through task_update so trimming/dedup rules apply.
pub fn create_task(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    template_name: &str,
) -> Result<TaskDto> {
    let cfg = load(wd)?;
    let tpl = find(&cfg.tasks, template_name, "task", |t| &t.name)?;
    let dto = tasks::task_create_with(
        conn,
        project_id,
        &tpl.title,
        None,
        tpl.priority.clone(),
        tpl.checklist.clone(),
        None,
    )?;
    match &tpl.tags {
        Some(tags) => tasks::task_update(
            conn,
            dto.id,
            tasks::TaskPatch {
                tags: Some(tags.clone()),
                ..Default::default()
            },
        ),
        None => Ok(dto),
    }
}

// Creates the note under Notes/ with the template content; a bare name gets
// ".md", and a name clash gets the " copy" / " copy N" suffix like notes_copy.
pub fn create_note(wd: &Path, template_name: &str, note_name: Option<&str>) -> Result<FsEntry> {
    let cfg = load(wd)?;
    let tpl = find(&cfg.notes, template_name, "note", |t| &t.name)?;
    let base = note_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&tpl.name);
    let name = fs_ops::check_name(base)?;
    let name = if name.contains('.') {
        name
    } else {
        format!("{name}.md")
    };
    let root = notes::vault_root(wd)?;
    let rel = fs_ops::unique_name(&root, &name, false);
    notes::create(wd, &rel, Some(&tpl.content))
}

pub fn document(wd: &Path, template_name: &str) -> Result<DocumentDto> {
    let cfg = load(wd)?;
    let tpl = find(&cfg.documents, template_name, "document", |t| &t.name)?;
    Ok(DocumentDto {
        name: tpl.name.clone(),
        markdown: tpl.markdown.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::onboarding::temp_test_dir;
    use serde_json::json;

    fn setup(tag: &str) -> PathBuf {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        wd
    }

    fn db() -> Connection {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        conn
    }

    fn write_templates(wd: &Path, value: Value) {
        fs::create_dir_all(paths::config_dir(wd)).unwrap();
        fs::write(config_file(wd), serde_json::to_string(&value).unwrap()).unwrap();
    }

    #[test]
    fn defaults_are_seeded_on_first_use_and_listed() {
        let wd = setup("tpl-defaults");
        assert!(!config_file(&wd).exists());
        let s = summary(&wd).unwrap();
        assert!(config_file(&wd).exists(), "first use must create the file");
        assert!(!s.projects.is_empty());
        assert!(!s.tasks.is_empty());
        assert!(!s.notes.is_empty());
        assert!(!s.documents.is_empty());
        assert_eq!(s.tasks[0].name, "Позвонить клиенту");
        assert!(!s.projects[0].description.is_empty());

        let cfg = load(&wd).unwrap();
        assert_eq!(cfg.tasks[0].title, "Позвонить клиенту");
        assert_eq!(cfg.tasks[0].priority.as_deref(), Some("high"));
        assert!(cfg.projects[0].files.iter().any(|f| f.path == paths::DATAS_MD));
        assert!(cfg.documents[0].markdown.contains("Коммерческое")
            || cfg.documents[0].name.contains("Коммерческое"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn unknown_top_level_keys_survive_app_rewrites() {
        let wd = setup("tpl-unknown-keys");
        write_templates(
            &wd,
            json!({ "projects": [], "customFuture": { "x": 1 }, "documents": [] }),
        );
        let cfg = load(&wd).unwrap();
        assert!(cfg.projects.is_empty());
        save(&wd, &cfg).unwrap();
        let raw: Value = serde_json::from_str(&fs::read_to_string(config_file(&wd)).unwrap()).unwrap();
        assert_eq!(raw["customFuture"]["x"], 1);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn create_project_from_template_makes_scaffolding_and_files() {
        let wd = setup("tpl-project");
        write_templates(
            &wd,
            json!({ "projects": [{
                "name": "Шаблон",
                "description": "тест",
                "files": [
                    { "path": "datas.md", "content": "данные шаблона" },
                    { "path": "КП/черновик.md", "content": "x" }
                ]
            }] }),
        );
        let conn = db();
        let dto = create_project(&conn, &wd, "Шаблон", None).unwrap();
        assert_eq!(dto.name, "Шаблон");
        let root = paths::projects_dir(&wd).join("Шаблон");
        assert_eq!(fs::read_to_string(root.join("datas.md")).unwrap(), "данные шаблона");
        assert!(root.join("КП/черновик.md").is_file());
        assert!(root.join(paths::PROJECT_TASKS).is_dir());
        assert!(root.join(paths::PROJECT_INBOX).is_dir());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM projects WHERE id = ?1", rusqlite::params![dto.id], |r| r.get::<_, i64>(0)).unwrap(),
            1
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn create_project_handles_custom_and_unique_names() {
        let wd = setup("tpl-project-names");
        write_templates(
            &wd,
            json!({ "projects": [{ "name": "Шаблон", "files": [] }] }),
        );
        let conn = db();
        let first = create_project(&conn, &wd, "Шаблон", None).unwrap();
        assert_eq!(first.name, "Шаблон");
        // A clash gets " 2", not an error.
        let second = create_project(&conn, &wd, "Шаблон", None).unwrap();
        assert_eq!(second.name, "Шаблон 2");
        // An explicit name wins over the template name.
        let third = create_project(&conn, &wd, "Шаблон", Some("  Свой  ")).unwrap();
        assert_eq!(third.name, "Свой");
        // Invalid explicit names are rejected like everywhere else.
        assert!(matches!(
            create_project(&conn, &wd, "Шаблон", Some("a/b")),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn project_template_file_paths_must_stay_inside_the_project() {
        let wd = setup("tpl-project-escape");
        for bad in ["../evil.md", "/etc/passwd", "a/../../x.md", ""] {
            write_templates(
                &wd,
                json!({ "projects": [{ "name": "Злой", "files": [{ "path": bad, "content": "x" }] }] }),
            );
            let conn = db();
            assert!(
                matches!(create_project(&conn, &wd, "Злой", None), Err(AppError::Invalid(_))),
                "accepted path {bad:?}"
            );
            assert!(!wd.join("evil.md").exists());
            // Nothing was created at all: paths are validated up front.
            assert!(!paths::projects_dir(&wd).join("Злой").exists());
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn create_task_applies_template_fields() {
        let wd = setup("tpl-task");
        write_templates(
            &wd,
            json!({ "tasks": [{
                "name": "Позвонить клиенту",
                "title": "Позвонить клиенту",
                "priority": "high",
                "checklist": [{ "text": " раз ", "done": false }, { "text": "  ", "done": true }],
                "tags": [" клиент ", ""]
            }] }),
        );
        let conn = db();
        let project = projects::create_project(&conn, &wd, "p1").unwrap();
        let t = create_task(&conn, &wd, project.id, "Позвонить клиенту").unwrap();
        assert_eq!(t.title, "Позвонить клиенту");
        assert_eq!(t.status, "new");
        assert_eq!(t.priority, "high");
        assert_eq!(t.checklist, vec![ChecklistItem { text: "раз".into(), done: false }]);
        assert_eq!(t.tags, vec!["клиент"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn create_note_writes_content_and_keeps_names_unique() {
        let wd = setup("tpl-note");
        write_templates(
            &wd,
            json!({ "notes": [{ "name": "Встреча", "content": "текст ноты" }] }),
        );
        let created = create_note(&wd, "Встреча", None).unwrap();
        assert_eq!(created.rel_path, "Встреча.md");
        assert_eq!(notes::read(&wd, "Встреча.md").unwrap(), "текст ноты");
        // A clash gets the copy suffix instead of an error.
        let again = create_note(&wd, "Встреча", None).unwrap();
        assert_eq!(again.rel_path, "Встреча copy.md");
        // A custom name keeps its own extension and gets ".md" without one.
        let custom = create_note(&wd, "Встреча", Some(" Итоги 2026 ")).unwrap();
        assert_eq!(custom.rel_path, "Итоги 2026.md");
        let keep_ext = create_note(&wd, "Встреча", Some("план.txt")).unwrap();
        assert_eq!(keep_ext.rel_path, "план.txt");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn document_returns_the_template_markdown() {
        let wd = setup("tpl-document");
        write_templates(
            &wd,
            json!({ "documents": [{ "name": "КП", "markdown": "# КП\n\nтекст" }] }),
        );
        let dto = document(&wd, "КП").unwrap();
        assert_eq!(dto.name, "КП");
        assert_eq!(dto.markdown, "# КП\n\nтекст");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn unknown_template_names_are_not_found() {
        let wd = setup("tpl-missing");
        let conn = db();
        let project = projects::create_project(&conn, &wd, "p1").unwrap();
        for message in [
            create_project(&conn, &wd, "нет", None).unwrap_err().to_string(),
            create_task(&conn, &wd, project.id, "нет").unwrap_err().to_string(),
            create_note(&wd, "нет", None).unwrap_err().to_string(),
            document(&wd, "нет").unwrap_err().to_string(),
        ] {
            assert!(message.contains("not found"), "{message}");
        }
        fs::remove_dir_all(&wd).unwrap();
    }
}

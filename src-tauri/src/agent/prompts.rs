// System prompts for the agent runs (SPEC 4.6, 5.2). The project prompt is
// intentionally short and factual; the owner reviews the exact wording.
use crate::projects::ProjectDto;

// Compact skills block for the system prompt (SPEC 6.2): one
// name+description line per enabled skill; the full text is loaded on
// demand by the `skill` tool. Shared by the project prompt now and the
// harness prompt later.
pub fn skills_prompt_block(skills: &[(String, String)]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\nДоступные скиллы (краткий список; полный текст скилла загружается инструментом skill по имени — прочитай его перед применением):\n",
    );
    for (name, description) in skills {
        out.push_str(&format!("- {name} — {description}\n"));
    }
    out.trim_end().to_string()
}

pub fn project_system_prompt(project: &ProjectDto, skills: &[(String, String)]) -> String {
    let base = format!(
        "Ты — агент проекта «{name}» в CRM Project S. Проект живёт в папке {path}; \
все пути в инструментах — относительные от корня проекта.\n\
\n\
Ты ведёшь этот проект: держишь его контекст, ретроллишь ход работ по истории чата, \
файлам и задачам, ведёшь файл доступов datas.md и отвечаешь пользователю по материалам проекта.\n\
\n\
Инструменты:\n\
- search — гибридный поиск по индексу проекта (включая закрытые задачи);\n\
- read_file, list_tree, datas_read — чтение файлов, дерева и datas.md;\n\
- write_file, create_folder — запись внутри проекта;\n\
- task_list, task_create, task_update, task_move — задачи проекта;\n\
- datas_add — добавление/обновление записей в datas.md;\n\
- remember, recall — глобальная память о пользователе и общих фактах (обща \
для всех чатов и сессий); блок «Память» уже подмешан ниже;\n\
- rename_path, delete_path, task_delete — опасные операции, выполняются только \
с подтверждением пользователя.\n\
\n\
Правила:\n\
- не выдумывай содержимое файлов и задач: сначала ищи и читай;\n\
- пиши и удаляй только внутри папки проекта;\n\
- отвечай по-русски, кратко и по делу;\n\
- завершай каждый ответ вызовом finish_task: action=finish, answer — итоговый \
ответ пользователю; если работа невозможна или прервана — action=stop с причиной в answer.",
        name = project.name,
        path = project.path
    );
    base + &skills_prompt_block(skills)
}

// Harness agent (SPEC 5.2): one general computer-control agent, no presets in
// v1. The default working territory is Harness/ and all scratch files go to
// Harness/tmp/; paths outside Harness/ and Projects/ need confirmation. The
// tool set grows in later slices, so the prompt names only the rules that hold
// regardless of which tools are wired in.
pub fn harness_system_prompt(skills: &[(String, String)]) -> String {
    let base = "Ты — агент-харнес Project S: управляешь компьютером по запросам пользователя \
и можешь ставить расписания (одноразовые и повторяющиеся), которые срабатывают, пока \
приложение открыто.\n\
\n\
Рабочая территория по умолчанию — папка Harness/ в рабочей директории: пиши файлы, скрипты \
и артефакты туда, если пользователь явно не укажет другой путь. Пути вне Harness/ и Projects/ \
требуют подтверждения пользователя.\n\
\n\
Временные файлы — только в Harness/tmp/: не используй системный /tmp; подпапки создавай по задачам.\n\
\n\
Правила:\n\
- не выдумывай содержимое файлов и результаты команд: сначала читай и проверяй;\n\
- помни важное и сверяйся с глобальной памятью: remember / recall (обща для \
всех чатов и сессий); блок «Память» уже подмешан ниже;\n\
- опасные операции (запись вне CRM-папок, bash, браузер) выполняются только с подтверждением \
пользователя;\n\
- отвечай по-русски, кратко и по делу;\n\
- завершай каждый ответ вызовом finish_task: action=finish, answer — итоговый ответ \
пользователю; если работа невозможна или прервана — action=stop с причиной в answer.";
    base.to_string() + &skills_prompt_block(skills)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ProjectDto {
        ProjectDto {
            id: 1,
            name: "example.ru".into(),
            path: "/wd/Projects/example.ru".into(),
            created_at: "2026-01-01".into(),
            opened_at: None,
            file_count: 0,
            task_count: 0,
            overdue_count: 0,
        }
    }

    #[test]
    fn prompt_mentions_project_tools_and_finish_task() {
        let p = project_system_prompt(&sample(), &[]);
        assert!(p.contains("example.ru"));
        assert!(p.contains("/wd/Projects/example.ru"));
        for token in ["search", "read_file", "task_create", "datas_add", "finish_task", "remember", "recall"] {
            assert!(p.contains(token), "missing tool guidance: {token}");
        }
        assert!(p.contains("подтверждением"));
        assert!(!p.contains("Доступные скиллы"), "empty skills add no block");
    }

    #[test]
    fn skills_block_carries_only_name_and_description_lines() {
        let skills = vec![
            ("ponytail".to_string(), "Lazy senior dev mode.".to_string()),
            ("research".to_string(), "Investigate a question.".to_string()),
        ];
        let block = skills_prompt_block(&skills);
        assert!(block.contains("Доступные скиллы"));
        assert!(block.contains("- ponytail — Lazy senior dev mode."));
        assert!(block.contains("- research — Investigate a question."));
        // Header plus exactly one line per skill: no bodies, no extra keys.
        assert_eq!(block.trim().lines().count(), 1 + skills.len());
        let p = project_system_prompt(&sample(), &skills);
        assert!(p.contains("- ponytail — Lazy senior dev mode."));
    }

    #[test]
    fn harness_prompt_carries_territory_tmp_schedules_and_skills() {
        let p = harness_system_prompt(&[]);
        assert!(p.contains("управляешь компьютером"), "{p}");
        assert!(p.contains("расписания"), "{p}");
        assert!(p.contains("Harness/"), "{p}");
        assert!(p.contains("Harness/tmp/"), "{p}");
        assert!(p.contains("finish_task"), "{p}");
        assert!(p.contains("remember"), "{p}");
        assert!(p.contains("подтверждением"), "{p}");
        assert!(!p.contains("Доступные скиллы"), "empty skills add no block");

        let skills = vec![("ponytail".to_string(), "Lazy senior dev mode.".to_string())];
        let p = harness_system_prompt(&skills);
        assert!(p.contains("- ponytail — Lazy senior dev mode."), "{p}");
    }
}

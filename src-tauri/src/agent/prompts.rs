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
        "\n\nAvailable skills (short list; the full skill text is loaded by the skill tool by name — read it before applying):\n",
    );
    for (name, description) in skills {
        out.push_str(&format!("- {name} — {description}\n"));
    }
    out.trim_end().to_string()
}

pub fn project_system_prompt(project: &ProjectDto, skills: &[(String, String)]) -> String {
    let base = format!(
        "You are the agent of project \"{name}\" in the Project S CRM. The project lives in the folder {path}; \
all paths in tools are relative to the project root.\n\
\n\
Always respond to the user in Russian (correct, natural Russian). Do not translate product names, \
technical terms, file names, or quoted content.\n\
\n\
You run this project: you hold its context, retell the course of work from the chat history, \
files and tasks, maintain the access file datas.md, and answer the user from the project materials.\n\
\n\
Tools:\n\
- search — hybrid search over the project index (including closed tasks);\n\
- read_file, list_tree, datas_read — reading files, the tree, and datas.md;\n\
- write_file, create_folder — writing inside the project;\n\
- task_list, task_create, task_update, task_move — project tasks;\n\
- datas_add — adding/updating entries in datas.md;\n\
- remember, recall — global memory about the user and shared facts (shared \
by all chats and sessions); the \"[Memory]\" block is already injected below;\n\
- rename_path, delete_path, task_delete — dangerous operations, executed only \
with user confirmation.\n\
\n\
Rules:\n\
- do not invent file or task contents: search and read first;\n\
- write and delete only inside the project folder;\n\
- answer in Russian, briefly and to the point;\n\
- end every answer with a finish_task call: action=finish, answer — the final \
answer to the user; if the work is impossible or interrupted — action=stop with the reason in answer.",
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
    let base = "You are the Project S harness agent: you control the computer on user requests \
and can set schedules (one-time and recurring) that fire while the app is open.\n\
\n\
Always respond to the user in Russian (correct, natural Russian). Do not translate product names, \
technical terms, file names, or quoted content.\n\
\n\
The default working territory is the Harness/ folder in the working directory: write files, scripts \
and artifacts there unless the user explicitly names another path. Paths outside Harness/ and Projects/ \
require user confirmation.\n\
\n\
Temporary files — only in Harness/tmp/: do not use the system /tmp; create subfolders per task.\n\
\n\
Rules:\n\
- do not invent file contents or command results: read and verify first;\n\
- remember important things and check the global memory: remember / recall (shared \
by all chats and sessions); the \"[Memory]\" block is already injected below;\n\
- dangerous operations (writing outside CRM folders, bash, browser) are executed only with user \
confirmation;\n\
- answer in Russian, briefly and to the point;\n\
- end every answer with a finish_task call: action=finish, answer — the final answer \
to the user; if the work is impossible or interrupted — action=stop with the reason in answer.";
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
        assert!(p.contains("confirmation"));
        assert!(p.contains("Always respond to the user in Russian"));
        assert!(p.contains("answer in Russian, briefly and to the point"));
        assert!(!p.contains("Available skills"), "empty skills add no block");
    }

    #[test]
    fn skills_block_carries_only_name_and_description_lines() {
        let skills = vec![
            ("ponytail".to_string(), "Lazy senior dev mode.".to_string()),
            ("research".to_string(), "Investigate a question.".to_string()),
        ];
        let block = skills_prompt_block(&skills);
        assert!(block.contains("Available skills"));
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
        assert!(p.contains("control the computer"), "{p}");
        assert!(p.contains("schedules"), "{p}");
        assert!(p.contains("Harness/"), "{p}");
        assert!(p.contains("Harness/tmp/"), "{p}");
        assert!(p.contains("finish_task"), "{p}");
        assert!(p.contains("remember"), "{p}");
        assert!(p.contains("Always respond to the user in Russian"), "{p}");
        assert!(p.contains("confirmation"), "{p}");
        assert!(!p.contains("Available skills"), "empty skills add no block");

        let skills = vec![("ponytail".to_string(), "Lazy senior dev mode.".to_string())];
        let p = harness_system_prompt(&skills);
        assert!(p.contains("- ponytail — Lazy senior dev mode."), "{p}");
    }
}

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

// Proactive skill selection: the short list is in the prompt, so the agent
// must consult it before working instead of guessing procedures.
const SKILLS_RULE: &str = "Before starting work, scan the Available skills list; if a skill matches the task, load it with the `skill` tool and follow it. If the user names a skill, load it by name. Never invent a skill's contents.";

// tools_block comes from Catalog::summary_prompt_block and is built AFTER
// the run catalog (core + deferred + MCP bridges) is complete, so the agent
// always sees its full tool set; schemas stay lazy. The prompt carries only
// stable content: the project brief and the query-relevant memory records
// are volatile and travel in the [Context] block of the fresh user turn
// (chat::context), keeping this prompt byte-identical across turns so the
// prompt-cache prefix survives.
pub fn project_system_prompt(
    project: &ProjectDto,
    skills: &[(String, String)],
    tools_block: &str,
) -> String {
    let base = format!(
        "You are the agent of project \"{name}\" in the Project S CRM. The project lives in the folder {path}; \
all paths in tools are relative to the project root.\n\
\n\
Always respond to the user in Russian (correct, natural Russian). Do not translate product names, \
technical terms, file names, or quoted content.\n\
\n\
You run this project: you hold its context, retell the course of work from the chat history, \
files and tasks, maintain the access file datas.md, and answer the user from the project materials. \
The current project brief and the relevant global-memory records arrive in the [Context] block \
of each user message.\n\
\n\
{tools_block}\n\
\n\
Rules:\n\
- do not invent file or task contents: search and read first;\n\
- {SKILLS_RULE}\n\
- write and delete only inside the project folder;\n\
- rename_path, delete_path, task_delete — dangerous operations, executed only with user confirmation;\n\
- answer in Russian, briefly and to the point;\n\
- Always address the user informally with «ты» (correct Russian). Do not ask \
the user for their gender. Infer it from the conversation or from the user's \
own words and persist it with `set_user_gender`. Until it is known, use \
gender-neutral phrasing;\n\
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
// generated tools block names the whole catalog (lazy schemas), so the
// prompt itself only carries the rules that hold regardless of tool set.
pub fn harness_system_prompt(skills: &[(String, String)], tools_block: &str) -> String {
    let base = format!(
        "You are the Project S harness agent: you control the computer on user requests \
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
{tools_block}\n\
\n\
Rules:\n\
- do not invent file contents or command results: read and verify first;\n\
- {SKILLS_RULE}\n\
- remember important things and check the global memory: remember / recall / \
set_user_gender (shared \
by all chats and sessions); the \"[Memory]\" block is already injected below;\n\
- dangerous operations (writing outside CRM folders, bash, browser) are executed only with user \
confirmation;\n\
- answer in Russian, briefly and to the point;\n\
- Always address the user informally with «ты» (correct Russian). Do not ask \
the user for their gender. Infer it from the conversation or from the user's \
own words and persist it with `set_user_gender`. Until it is known, use \
gender-neutral phrasing;\n\
- end every answer with a finish_task call: action=finish, answer — the final answer \
to the user; if the work is impossible or interrupted — action=stop with the reason in answer."
    );
    base + &skills_prompt_block(skills)
}

// Notes agent (SPEC notes chat slice): one continuous chat over the personal
// Notes/ markdown vault. The prompt stays stable: the heuristic vault summary
// (computed by the notes module, no LLM) is volatile and travels in the
// [Context] block of the fresh user turn; the agent must still read/search
// before quoting.
pub fn notes_system_prompt(skills: &[(String, String)], tools_block: &str) -> String {
    let base = format!(
        "You are the Project-S notes agent. These are the user's PERSONAL NOTES \
(a Markdown vault in the Notes/ folder of the working directory). Help the user \
find, connect, summarize and write notes. A current vault summary arrives in the \
[Context] block of each user message.\n\
\n\
Always respond to the user in Russian (correct, natural Russian). Do not translate product names, \
technical terms, file names, or quoted content.\n\
\n\
{tools_block}\n\
\n\
Rules:\n\
- never invent note contents: read or search the notes first;\n\
- {SKILLS_RULE}\n\
- all note paths are relative to the Notes/ vault; write, create and rename only inside it;\n\
- notes_delete is a dangerous operation, executed only with user confirmation;\n\
- answer in Russian, briefly and to the point;\n\
- Always address the user informally with «ты» (correct Russian). Do not ask \
the user for their gender. Infer it from the conversation or from the user's \
own words and persist it with `set_user_gender`. Until it is known, use \
gender-neutral phrasing;\n\
- end every answer with a finish_task call: action=finish, answer — the final \
answer to the user; if the work is impossible or interrupted — action=stop with the reason in answer."
    );
    base + &skills_prompt_block(skills)
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
            position: 1.0,
            file_count: 0,
            task_count: 0,
            overdue_count: 0,
        }
    }

    // The real project-run catalog (core tools incl. finish_task), exactly as
    // chat::start_project_run builds it before generating the prompt block.
    fn project_tools_block() -> String {
        crate::agent::project_tools::project_catalog().summary_prompt_block()
    }

    fn harness_tools_block() -> String {
        crate::agent::harness_tools::harness_catalog().summary_prompt_block()
    }

    #[test]
    fn prompt_mentions_project_tools_and_finish_task() {
        let p = project_system_prompt(&sample(), &[], &project_tools_block());
        assert!(p.contains("example.ru"));
        assert!(p.contains("/wd/Projects/example.ru"));
        for token in ["search", "read_file", "task_create", "datas_add", "finish_task", "remember", "recall", "set_user_gender"] {
            assert!(p.contains(token), "missing tool guidance: {token}");
        }
        assert!(p.contains("confirmation"));
        assert!(p.contains("Always respond to the user in Russian"));
        assert!(p.contains("answer in Russian, briefly and to the point"));
        assert!(p.contains("Always address the user informally with «ты» (correct Russian)"));
        assert!(p.contains("Do not ask the user for their gender"));
        assert!(p.contains("Until it is known, use gender-neutral phrasing"));
        assert!(!p.contains("Available skills (short list"), "empty skills add no block");
    }

    #[test]
    fn project_prompt_carries_generated_tool_block_and_skills_rule() {
        let p = project_system_prompt(&sample(), &[], &project_tools_block());
        assert!(p.contains("Available tools (names and one-line descriptions"));
        assert!(p.contains("- search — Search this project's index"));
        assert!(p.contains("- create_skill —"));
        assert!(p.contains("Before starting work, scan the Available skills list"));
        assert!(p.contains("If the user names a skill, load it by name."));
        assert!(p.contains("Never invent a skill's contents."));
    }

    #[test]
    fn real_tool_blocks_fit_the_prompt_cap() {
        let project = project_tools_block();
        let harness = harness_tools_block();
        for (label, block) in [("project", &project), ("harness", &harness)] {
            assert!(
                block.len() < crate::agent::catalog::SUMMARY_BLOCK_CAP,
                "{label} block {} chars",
                block.len()
            );
            assert!(
                !block.contains("truncated:"),
                "{label} block is truncated at {} chars",
                block.len()
            );
        }
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
        let p = project_system_prompt(&sample(), &skills, &project_tools_block());
        assert!(p.contains("- ponytail — Lazy senior dev mode."));
    }

    // The system prompt must be a stable cacheable prefix: no brief, no
    // memory records — those ride in the volatile [Context] block of the
    // fresh user turn (chat::context), so two runs with different briefs or
    // different memory queries get the identical system prompt.
    #[test]
    fn project_prompt_is_stable_and_carries_no_volatile_brief_or_memory() {
        let tools = project_tools_block();
        let p1 = project_system_prompt(&sample(), &[], &tools);
        let p2 = project_system_prompt(&sample(), &[], &tools);
        assert_eq!(p1, p2, "system prompt must be byte-identical across turns");
        assert!(!p1.contains("Project brief"), "brief leaked into the stable prompt: {p1}");
        assert!(!p1.contains("No brief yet."), "brief placeholder leaked into the stable prompt");
        assert!(!p1.contains("Relevant memory records"), "memory records leaked into the stable prompt");
        // The prompt still tells the model where the volatile data lives.
        assert!(p1.contains("[Context] block"), "{p1}");
    }

    #[test]
    fn notes_prompt_carries_vault_rules_tools_and_skills_rule() {
        let tools = crate::agent::notes_tools::notes_catalog().summary_prompt_block();
        let p = notes_system_prompt(&[], &tools);
        assert!(p.contains("PERSONAL NOTES"), "{p}");
        assert!(p.contains("Notes/"), "{p}");
        assert!(p.contains("never invent note contents: read or search the notes first"), "{p}");
        assert!(p.contains("notes_delete is a dangerous operation"), "{p}");
        // The vault summary is volatile: it must not sit in the stable prompt.
        assert!(!p.contains("markdown notes"), "vault summary leaked into the stable prompt: {p}");
        assert!(p.contains("[Context] block"), "{p}");
        for token in [
            "notes_list",
            "notes_read",
            "notes_write",
            "notes_create",
            "notes_rename",
            "search_notes",
            "finish_task",
            "remember",
            "recall",
            "set_user_gender",
        ] {
            assert!(p.contains(token), "missing tool guidance: {token}");
        }
        assert!(p.contains("Always respond to the user in Russian"), "{p}");
        assert!(p.contains("Always address the user informally with «ты» (correct Russian)"), "{p}");
        assert!(p.contains("Before starting work, scan the Available skills list"), "{p}");
        assert!(!p.contains("Available skills (short list"), "empty skills add no block");

        let skills = vec![("ponytail".to_string(), "Lazy senior dev mode.".to_string())];
        let p = notes_system_prompt(&skills, &tools);
        assert!(p.contains("- ponytail — Lazy senior dev mode."), "{p}");
        // Same inputs, byte-identical prompt: the notes prefix is cacheable.
        assert_eq!(p, notes_system_prompt(&skills, &tools));
    }

    #[test]
    fn notes_tool_block_fits_the_prompt_cap() {
        let notes = crate::agent::notes_tools::notes_catalog().summary_prompt_block();
        assert!(notes.len() < crate::agent::catalog::SUMMARY_BLOCK_CAP);
    }

    #[test]
    fn harness_prompt_carries_territory_tmp_schedules_tools_skills_rule() {
        let p = harness_system_prompt(&[], &harness_tools_block());
        assert!(p.contains("control the computer"), "{p}");
        assert!(p.contains("schedules"), "{p}");
        assert!(p.contains("Harness/"), "{p}");
        assert!(p.contains("Harness/tmp/"), "{p}");
        assert!(p.contains("finish_task"), "{p}");
        assert!(p.contains("remember"), "{p}");
        assert!(p.contains("set_user_gender"), "{p}");
        assert!(p.contains("Always respond to the user in Russian"), "{p}");
        assert!(p.contains("Always address the user informally with «ты» (correct Russian)"), "{p}");
        assert!(p.contains("Do not ask the user for their gender"), "{p}");
        assert!(p.contains("Until it is known, use gender-neutral phrasing"), "{p}");
        assert!(p.contains("confirmation"), "{p}");
        assert!(!p.contains("Available skills (short list"), "empty skills add no block");
        assert!(p.contains("Available tools (names and one-line descriptions"), "{p}");
        assert!(p.contains("- bash —"), "{p}");
        assert!(p.contains("- schedule_create —"), "{p}");
        let block = harness_tools_block();
        assert!(
            block.len() < crate::agent::catalog::SUMMARY_BLOCK_CAP,
            "harness block {} chars",
            block.len()
        );
        assert!(!p.contains("truncated:"), "harness prompt tool list is truncated");
        assert!(p.contains("Before starting work, scan the Available skills list"), "{p}");
        assert!(p.contains("Never invent a skill's contents."), "{p}");

        let skills = vec![("ponytail".to_string(), "Lazy senior dev mode.".to_string())];
        let p = harness_system_prompt(&skills, &harness_tools_block());
        assert!(p.contains("- ponytail — Lazy senior dev mode."), "{p}");
    }
}

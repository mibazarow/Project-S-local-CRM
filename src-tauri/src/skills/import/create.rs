// User skill creation and the `create_skill` agent tool: write
// Settings/skills/user/<name>/SKILL.md through the same validation the
// loader applies, refusing builtin names and (without `overwrite`) existing
// user skills.

use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::paths;
use super::super::manifest::{discover, skill_info, SkillInfo};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// Creates a user skill (SPEC 6.2 "создание скиллов"): writes
// Settings/skills/user/<name>/SKILL.md and returns it once the loader
// confirms it parses. An existing user skill is only replaced with
// `overwrite`; a builtin of the same name is always refused (the user dir
// would shadow it, which the conflict rules already surface as an error).
pub fn create(
    wd: &Path,
    name: &str,
    description: &str,
    kind: Option<&str>,
    body: Option<&str>,
    overwrite: bool,
) -> Result<SkillInfo> {
    if !valid_skill_name(name) {
        return Err(AppError::Invalid(format!(
            "invalid skill name \"{name}\": use lowercase letters, digits and hyphens"
        )));
    }
    let description = description.trim();
    if description.is_empty() || description.contains('\n') {
        return Err(AppError::Invalid("description must be one non-empty line".into()));
    }
    let kind = match kind.unwrap_or("instruction") {
        "instruction" => None,
        k @ ("script" | "extension") => Some(k),
        other => {
            return Err(AppError::Invalid(format!(
                "unknown skill type \"{other}\" (instruction|script|extension)"
            )))
        }
    };
    if paths::skills_builtin_dir(wd).join(name).join(paths::SKILL_FILE).is_file() {
        return Err(AppError::Invalid(format!(
            "\"{name}\" conflicts with a builtin skill"
        )));
    }
    let dest = paths::skills_user_dir(wd).join(name);
    if dest.exists() {
        if !overwrite {
            return Err(AppError::Invalid(format!(
                "skill \"{name}\" already exists (pass overwrite to replace)"
            )));
        }
        fs::remove_dir_all(&dest)?;
    }
    let mut fm = format!("---\nname: {name}\ndescription: {description}\n");
    if let Some(k) = kind {
        fm.push_str(&format!("type: {k}\n"));
    }
    fm.push_str("---\n");
    let body = body.unwrap_or("").trim();
    let text = if body.is_empty() {
        fm
    } else {
        format!("{fm}\n{body}\n")
    };
    fs::create_dir_all(&dest)?;
    fs::write(dest.join(paths::SKILL_FILE), &text)?;
    match discover(wd).skills.iter().find(|s| s.name == name) {
        Some(manifest) => Ok(skill_info(manifest)),
        None => {
            let _ = fs::remove_dir_all(&dest);
            Err(AppError::Invalid("created skill failed validation in the loader".into()))
        }
    }
}

// The `create_skill` agent tool: creates a user skill through the same
// `create` logic the UI uses. Mutating with the Check policy (SPEC 6.2 Q20):
// every agent-initiated skill creation goes through the standard
// confirmation dialog — no silent skill creation, whether the user asked
// for a skill in chat or accepted a repeat-detector proposal. It only ever
// writes inside Settings/skills/user/. An existing user skill is replaced
// only with an explicit overwrite=true; builtin names are always refused.
pub struct CreateSkillTool;

impl Tool for CreateSkillTool {
    fn name(&self) -> &str {
        "create_skill"
    }
    fn description(&self) -> &str {
        "Create a user skill saved to Settings/skills/user/<name>/SKILL.md; it then appears in the skills list and loads via the skill tool. Name: lowercase letters, digits and hyphens. kind defaults to instruction. Fails if the name exists or clashes with a builtin; pass overwrite=true to replace an existing user skill (builtins are never replaced)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "description": { "type": "string" },
                "kind": { "type": "string", "enum": ["instruction", "script", "extension"] },
                "body": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["name", "description"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        let name = args["name"].as_str().unwrap_or("?");
        Some(format!(
            "Создание скилла «{name}» в Settings/skills/user/ — агент пишет новый SKILL.md"
        ))
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = ctx.working_dir.clone().ok_or(AppError::NotConfigured)?;
        let name = args["name"].as_str().unwrap_or_default();
        let description = args["description"].as_str().unwrap_or_default();
        let kind = args.get("kind").and_then(Value::as_str);
        let body = args.get("body").and_then(Value::as_str);
        let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
        match create(&wd, name, description, kind, body, overwrite) {
            Ok(info) => Ok(ToolResult::ok(json!({ "skill": info }))),
            Err(e) => {
                let msg = e.to_string();
                let code = if msg.contains("conflicts with a builtin") {
                    ToolErrorCode::PermissionDenied
                } else if msg.contains("invalid skill name")
                    || msg.contains("description")
                    || msg.contains("unknown skill type")
                    || msg.contains("already exists")
                {
                    ToolErrorCode::InvalidArgs
                } else {
                    ToolErrorCode::ExecutionError
                };
                Ok(ToolResult::err(code, msg))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::testutil::{scaffold, write_skill};
    use crate::skills::{load_full, short_list};

    #[test]
    fn create_writes_a_skill_the_loader_discovers() {
        let wd = scaffold("skills-create");
        let info = create(&wd, "my-skill", "My new skill", None, Some("# Guide\n\nDo it."), false).unwrap();
        assert_eq!(info.name, "my-skill");
        assert_eq!(info.kind, "instruction");
        assert!(!info.builtin && info.enabled);
        let text = fs::read_to_string(paths::skills_user_dir(&wd).join("my-skill/SKILL.md")).unwrap();
        assert!(text.contains("name: my-skill"));
        assert_eq!(load_full(&wd, "my-skill").unwrap(), "# Guide\n\nDo it.");
        assert!(short_list(&wd).contains(&("my-skill".to_string(), "My new skill".to_string())));
        assert!(create(&wd, "Bad Name", "x", None, None, false).is_err());
        assert!(create(&wd, "-lead", "x", None, None, false).is_err());
        let err = create(&wd, "my-skill", "dup", None, None, false).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        // overwrite=true replaces the user copy
        create(&wd, "my-skill", "Replaced", None, Some("New body."), true).unwrap();
        assert_eq!(load_full(&wd, "my-skill").unwrap(), "New body.");
        // a builtin name is refused even with overwrite
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "builtin-one",
            "---\nname: builtin-one\ndescription: b\n---\nB.\n",
        );
        let err = create(&wd, "builtin-one", "x", None, None, true).unwrap_err().to_string();
        assert!(err.contains("conflicts with a builtin"), "{err}");
        assert!(create(&wd, "other", "d", Some("magic"), None, false).is_err());
        fs::remove_dir_all(&wd).unwrap();
    }

    // Q20 end-to-end policy check: a run that calls create_skill must pass
    // the confirmation gate (Check policy). Allow → the skill is created and
    // listed; deny → nothing is written.
    struct ScriptedBackend {
        rounds: std::sync::Mutex<std::collections::VecDeque<crate::llm::ChatOutcome>>,
    }

    impl crate::agent::r#loop::ChatBackend for ScriptedBackend {
        fn chat_stream(
            &self,
            _messages: &[crate::llm::ChatMessage],
            _tools: Option<&[crate::llm::ToolSpec]>,
            _params: &crate::llm::ChatParams,
            _on_event: &mut dyn FnMut(crate::llm::ChatEvent),
        ) -> Result<crate::llm::ChatOutcome> {
            self.rounds
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| AppError::Invalid("scripted backend exhausted".into()))
        }
    }

    fn create_skill_run(wd: &Path, confirmer: &dyn crate::confirm::Confirmer) -> String {
        use crate::agent::catalog::Catalog;
        use crate::agent::finish::FinishTaskTool;
        use crate::agent::r#loop as agent_loop;
        use crate::llm::{ChatMessage, ChatOutcome, ChatParams, ToolCall};
        use std::sync::Arc;

        let backend = ScriptedBackend {
            rounds: std::sync::Mutex::new(
                vec![
                    ChatOutcome {
                        content: None,
                        reasoning: None,
                        tool_calls: vec![ToolCall::new(
                            "c1".into(),
                            "create_skill".into(),
                            json!({ "name": "repeat-task", "description": "Repeated workflow", "body": "Steps." })
                                .to_string(),
                        )],
                        finish_reason: Some("tool_calls".into()),
                        usage: None,
                    },
                    ChatOutcome {
                        content: None,
                        reasoning: None,
                        tool_calls: vec![ToolCall::new(
                            "c2".into(),
                            "finish_task".into(),
                            json!({ "action": "finish", "answer": "готово" }).to_string(),
                        )],
                        finish_reason: Some("tool_calls".into()),
                        usage: None,
                    },
                ]
                .into(),
            ),
        };
        let mut catalog = Catalog::new();
        catalog.register(Arc::new(FinishTaskTool), true).unwrap();
        catalog.register(Arc::new(CreateSkillTool), true).unwrap();
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let config = agent_loop::AgentRunConfig {
            messages: vec![ChatMessage::system("sys"), ChatMessage::user("да, создай скилл")],
            max_steps: 8,
            limits: agent_loop::RunLimits::default(),
            allow_all: false,
            project_id: None,
            session_id: None,
            chat_id: None,
            params: ChatParams {
                model: "test-model".into(),
                temperature: None,
                max_tokens: None,
                reasoning_effort: None,
                requires_reasoning_passback: false,
            },
            cancel: None,
            state: None,
            working_dir: Some(wd.to_path_buf()),
            notes_root: None,
        };
        let result = agent_loop::run(&mut state, &config, confirmer, |_| {}).unwrap();
        result.answer
    }

    #[test]
    fn confirmed_create_skill_run_creates_and_lists_the_skill() {
        let wd = scaffold("skills-create-confirmed");
        let answer = create_skill_run(&wd, &crate::confirm::AutoConfirmer::allowing(true));
        assert_eq!(answer, "готово");
        assert!(paths::skills_user_dir(&wd).join("repeat-task/SKILL.md").is_file());
        assert!(short_list(&wd)
            .contains(&("repeat-task".to_string(), "Repeated workflow".to_string())));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn denied_create_skill_run_writes_nothing() {
        let wd = scaffold("skills-create-denied");
        let answer = create_skill_run(&wd, &crate::confirm::DenyAllConfirmer);
        assert_eq!(answer, "готово");
        assert!(!paths::skills_user_dir(&wd).join("repeat-task/SKILL.md").exists());
        assert!(short_list(&wd).is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn create_skill_tool_creates_skill_and_returns_error_envelopes() {
        let wd = scaffold("skills-create-tool");
        let ctx = ToolCtx {
            working_dir: Some(wd.clone()),
            ..Default::default()
        };
        let tool = CreateSkillTool;

        let r = tool
            .execute(
                &json!({ "name": "agent-guide", "description": "Guide", "body": "Do it." }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["skill"]["name"], "agent-guide");
        assert_eq!(load_full(&wd, "agent-guide").unwrap(), "Do it.");
        assert!(short_list(&wd).contains(&("agent-guide".to_string(), "Guide".to_string())));

        let r = tool
            .execute(&json!({ "name": "Bad Name", "description": "x" }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");

        let r = tool
            .execute(&json!({ "name": "agent-guide", "description": "dup" }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");

        // overwrite=true replaces the user copy
        let r = tool
            .execute(
                &json!({ "name": "agent-guide", "description": "Guide v2", "body": "New.", "overwrite": true }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(load_full(&wd, "agent-guide").unwrap(), "New.");

        // a builtin name is refused even with overwrite
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "builtin-one",
            "---\nname: builtin-one\ndescription: b\n---\nB.\n",
        );
        let r = tool
            .execute(
                &json!({ "name": "builtin-one", "description": "x", "overwrite": true }),
                &ctx,
            )
            .unwrap();
        assert_eq!(r.error.unwrap().code, "permission_denied");
        fs::remove_dir_all(&wd).unwrap();
    }
}

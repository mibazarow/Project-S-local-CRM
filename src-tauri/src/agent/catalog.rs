// Tool registry: duplicate names are a hard error, policy-hidden tools resolve
// to a "hidden: <reason>" outcome, and lazy discovery keeps only the core set
// active until enable_tools binds more (SPEC 6.4).
use super::tool::Tool;
use crate::error::{AppError, Result};
use crate::llm::ToolSpec;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub const LIST_TOOLS: &str = "list_available_tools";
pub const ENABLE_TOOLS: &str = "enable_tools";
pub const DEFAULT_SCHEMA_CAP: usize = 128;
// Max size of the generated tool-list prompt block (chars). Sized so the real
// project and harness tool blocks (incl. image_transform) fit in full without
// truncation; only a runaway MCP set would ever hit it.
pub const SUMMARY_BLOCK_CAP: usize = 20_000;

pub enum Resolution<'a> {
    Found(&'a Arc<dyn Tool>),
    Hidden(&'a str),
    Unknown,
}

pub enum EnableOutcome {
    Activated,
    AlreadyActive,
    Hidden(String),
    NotFound,
    LimitExceeded,
}

#[derive(Default)]
pub struct Catalog {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    hidden: BTreeMap<String, String>,
    active: BTreeSet<String>,
    schema_cap: usize,
}

impl Catalog {
    pub fn new() -> Self {
        Self::with_schema_cap(DEFAULT_SCHEMA_CAP)
    }

    pub fn with_schema_cap(cap: usize) -> Self {
        Catalog {
            schema_cap: cap,
            ..Default::default()
        }
    }

    // Duplicate names (and the reserved discovery names) fail registration
    // loudly instead of silently shadowing each other.
    pub fn register(&mut self, tool: Arc<dyn Tool>, core: bool) -> Result<()> {
        let name = tool.name().to_string();
        if name == LIST_TOOLS || name == ENABLE_TOOLS {
            return Err(AppError::Invalid(format!(
                "tool name {name} is reserved for lazy discovery"
            )));
        }
        if self.tools.contains_key(&name) {
            return Err(AppError::Invalid(format!("duplicate tool name: {name}")));
        }
        self.tools.insert(name.clone(), tool);
        if core {
            self.active.insert(name);
        }
        Ok(())
    }

    pub fn hide(&mut self, name: &str, reason: impl Into<String>) {
        self.hidden.insert(name.to_string(), reason.into());
        self.active.remove(name);
    }

    pub fn resolve(&self, name: &str) -> Resolution<'_> {
        if let Some(reason) = self.hidden.get(name) {
            return Resolution::Hidden(reason);
        }
        match self.tools.get(name) {
            Some(tool) => Resolution::Found(tool),
            None => Resolution::Unknown,
        }
    }

    // Specs for the active tool list; the two discovery meta-tools are always
    // exposed so the model can widen the set itself.
    pub fn to_specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .active
            .iter()
            .filter_map(|name| {
                self.tools.get(name).map(|t| ToolSpec {
                    name: name.clone(),
                    description: t.description().to_string(),
                    parameters: t.parameters(),
                })
            })
            .collect();
        specs.extend(discovery_specs());
        specs
    }

    pub fn list_summary(&self) -> Value {
        let active: Vec<&String> = self.active.iter().collect();
        let available: Vec<&String> = self
            .tools
            .keys()
            .filter(|name| !self.active.contains(*name) && !self.hidden.contains_key(*name))
            .collect();
        let mut hidden = Map::new();
        for (name, reason) in &self.hidden {
            hidden.insert(name.clone(), json!(reason));
        }
        json!({
            "active": active,
            "available": available,
            "hidden": hidden,
            "schema_cap": self.schema_cap,
        })
    }

    // The model-facing full tool list: every registered tool — active,
    // deferred and MCP-bridged — as (name, first line of description),
    // sorted by name. Hidden tools stay out: they are policy-disabled and
    // list_available_tools already reports them with their reason.
    pub fn full_summary(&self) -> Vec<(String, String)> {
        self.tools
            .iter()
            .filter(|(name, _)| !self.hidden.contains_key(name.as_str()))
            .map(|(name, tool)| {
                let first_line = tool.description().lines().next().unwrap_or("").trim();
                (name.clone(), first_line.to_string())
            })
            .collect()
    }

    // Compact system-prompt block built from full_summary: header plus one
    // line per tool. Capped so a huge MCP set cannot blow up the prompt;
    // truncation is stated honestly instead of silently dropping tools.
    pub fn summary_prompt_block(&self) -> String {
        let entries = self.full_summary();
        let mut out = String::from(
            "Available tools (names and one-line descriptions; schemas are loaded lazily — call list_available_tools/enable_tools when needed):\n",
        );
        let mut shown = 0usize;
        for (name, description) in &entries {
            let line = format!("- {name} — {description}\n");
            if out.len() + line.len() > SUMMARY_BLOCK_CAP {
                break;
            }
            out.push_str(&line);
            shown += 1;
        }
        if shown < entries.len() {
            out.push_str(&format!(
                "(truncated: {shown} of {} tools listed; call list_available_tools for the rest)\n",
                entries.len()
            ));
        }
        out.trim_end().to_string()
    }

    pub fn enable(&mut self, name: &str) -> EnableOutcome {
        if let Some(reason) = self.hidden.get(name) {
            return EnableOutcome::Hidden(reason.clone());
        }
        if !self.tools.contains_key(name) {
            return EnableOutcome::NotFound;
        }
        if self.active.contains(name) {
            return EnableOutcome::AlreadyActive;
        }
        // The cap counts the active tool specs plus the two discovery tools.
        if self.active.len() + 2 >= self.schema_cap {
            return EnableOutcome::LimitExceeded;
        }
        self.active.insert(name.to_string());
        EnableOutcome::Activated
    }
}

pub fn discovery_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: LIST_TOOLS.into(),
            description: "List the tool catalog: active tools, tools available to enable, and tools hidden by policy.".into(),
            parameters: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolSpec {
            name: ENABLE_TOOLS.into(),
            description: "Enable named tools from the catalog so they become callable in later rounds.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "tools": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["tools"],
                "additionalProperties": false
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::{EchoTool, PermissionPolicy, ToolCtx, ToolResult};

    struct NamedTool(&'static str);

    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "named test tool"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {}, "additionalProperties": false })
        }
        fn execute(&self, _args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
            Ok(ToolResult::ok(json!({})))
        }
    }

    #[test]
    fn duplicate_registration_is_a_hard_error() {
        let mut c = Catalog::new();
        c.register(Arc::new(EchoTool::new(PermissionPolicy::Skip)), true)
            .unwrap();
        let err = c
            .register(Arc::new(EchoTool::new(PermissionPolicy::Skip)), true)
            .unwrap_err();
        assert!(err.to_string().contains("duplicate tool name: echo"));
    }

    #[test]
    fn reserved_discovery_names_cannot_be_registered() {
        let mut c = Catalog::new();
        assert!(c.register(Arc::new(NamedTool(LIST_TOOLS)), true).is_err());
    }

    #[test]
    fn hidden_tool_resolves_to_hidden_not_unknown() {
        let mut c = Catalog::new();
        c.register(Arc::new(EchoTool::new(PermissionPolicy::Skip)), true)
            .unwrap();
        c.hide("echo", "disabled for this project");
        match c.resolve("echo") {
            Resolution::Hidden(reason) => assert_eq!(reason, "disabled for this project"),
            _ => panic!("expected hidden resolution"),
        }
        assert!(!c.to_specs().iter().any(|s| s.name == "echo"));
    }

    #[test]
    fn unknown_tool_resolves_to_unknown() {
        let c = Catalog::new();
        assert!(matches!(c.resolve("nope"), Resolution::Unknown));
    }

    #[test]
    fn lazy_discovery_enables_non_core_tools() {
        let mut c = Catalog::new();
        c.register(Arc::new(NamedTool("extra")), false).unwrap();
        assert!(!c.to_specs().iter().any(|s| s.name == "extra"));
        assert!(matches!(c.enable("extra"), EnableOutcome::Activated));
        assert!(c.to_specs().iter().any(|s| s.name == "extra"));
        assert!(matches!(c.enable("extra"), EnableOutcome::AlreadyActive));
        assert!(matches!(c.enable("ghost"), EnableOutcome::NotFound));
        c.hide("extra", "policy");
        assert!(matches!(c.enable("extra"), EnableOutcome::Hidden(_)));
    }

    #[test]
    fn enable_respects_schema_cap() {
        let mut c = Catalog::with_schema_cap(3);
        c.register(Arc::new(NamedTool("extra")), false).unwrap();
        // 1 active core + 2 discovery tools already fill the cap of 3.
        c.register(Arc::new(NamedTool("core1")), true).unwrap();
        assert!(matches!(c.enable("extra"), EnableOutcome::LimitExceeded));
    }

    #[test]
    fn discovery_tools_are_always_in_specs() {
        let c = Catalog::new();
        let names: Vec<String> = c.to_specs().into_iter().map(|s| s.name).collect();
        assert!(names.iter().any(|n| n == LIST_TOOLS));
        assert!(names.iter().any(|n| n == ENABLE_TOOLS));
    }

    struct DescTool {
        name: String,
        description: String,
    }

    impl Tool for DescTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            &self.description
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {}, "additionalProperties": false })
        }
        fn execute(&self, _args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
            Ok(ToolResult::ok(json!({})))
        }
    }

    fn desc_tool(name: &str, description: &str) -> Arc<dyn Tool> {
        Arc::new(DescTool {
            name: name.into(),
            description: description.into(),
        })
    }

    #[test]
    fn full_summary_covers_active_deferred_and_mcp_tools_sorted() {
        let mut c = Catalog::new();
        c.register(desc_tool("zeta_core", "core tool\nsecond line ignored"), true)
            .unwrap();
        c.register(desc_tool("alpha_deferred", "deferred tool"), false)
            .unwrap();
        // MCP-bridged tools register as core in the run catalog.
        c.register(desc_tool("mcp_srv_echo", "echoes via mcp"), true)
            .unwrap();
        c.hide("zeta_core", "disabled for this project");
        let summary = c.full_summary();
        let names: Vec<&str> = summary.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["alpha_deferred", "mcp_srv_echo"]);
        assert_eq!(summary[0].1, "deferred tool");
        // only the first description line reaches the prompt block
        c.register(desc_tool("multi", "first\nsecond"), true).unwrap();
        let summary = c.full_summary();
        let multi = summary.iter().find(|(n, _)| n == "multi").unwrap();
        assert_eq!(multi.1, "first");
    }

    #[test]
    fn summary_prompt_block_has_header_lines_and_honest_truncation() {
        let mut c = Catalog::new();
        c.register(desc_tool("alpha", "alpha does things"), false).unwrap();
        c.register(desc_tool("beta", "beta does things"), true).unwrap();
        let block = c.summary_prompt_block();
        assert!(block.starts_with("Available tools (names and one-line descriptions"));
        assert!(block.contains("- alpha — alpha does things"));
        assert!(block.contains("- beta — beta does things"));
        assert!(!block.contains("truncated"));

        // blow the cap: many tools with long one-line descriptions
        let mut big = Catalog::new();
        for i in 0..120 {
            big.register(
                desc_tool(&format!("tool_{i:03}"), &"d".repeat(200)),
                false,
            )
            .unwrap();
        }
        let block = big.summary_prompt_block();
        assert!(block.len() <= SUMMARY_BLOCK_CAP + 200, "len {}", block.len());
        assert!(block.contains("truncated: "), "{block}");
        assert!(block.contains("of 120 tools listed"));
    }
}

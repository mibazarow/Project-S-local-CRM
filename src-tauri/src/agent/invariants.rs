// Catalog invariants (SPEC 6.4): every tool in the project, harness and notes
// catalogs must be well-formed and safely guarded. This module only holds
// tests; it enforces the rules the loop relies on and catches regressions
// (a destructive tool that lost its confirmation, a schema that stopped
// rejecting unknown properties, a runaway description, etc.).
#![cfg(test)]

use super::catalog::{Catalog, Resolution, ENABLE_TOOLS, LIST_TOOLS};
use super::harness_tools::harness_catalog;
use super::notes_tools::notes_catalog;
use super::project_tools::project_catalog;
use super::tool::{PermissionPolicy, ToolClass};

// Max description length in chars. Raised above 600 to fit the longest
// legitimate tool guide (ssh_exec at 1257 chars); never truncate meaning.
const DESC_CAP: usize = 1300;
// Max serialized JSON-schema size in chars.
const SCHEMA_CAP: usize = 12_000;

// Mutating tools that are allowed to carry PermissionPolicy::Skip. Each entry
// is reviewed: the tool mutates state but is guarded by something other than
// the confirmation queue (a strict path resolver inside a bound root, a
// per-call CheckConditional, or it only touches the model's own scratch/plan
// or the user's own memory), so a confirmation would add nothing. Anything
// destructive that is NOT listed here must confirm (non-Skip policy).
const MUTATING_SKIP_ALLOWLIST: &[&str] = &[
    // Task board edits (project + harness crm twin): scoped to the bound
    // project, reversible, not destructive to files.
    "task_create",
    "task_update",
    "task_move",
    "crm_task_create",
    "crm_task_update",
    "crm_task_move",
    // datas.md append: additive section write inside the bound project root.
    "datas_add",
    // Saving an SSH profile writes an additive section into the bound
    // project's datas.md (same guard as datas_add), never a destructive op.
    "ssh_save_profile_to_datas",
    // File creation inside the strictly-resolved project/vault root: cannot
    // escape the bound root, so no confirmation needed.
    "write_file",
    "create_folder",
    "edit_file",
    "create_document",
    "create_spreadsheet",
    // Notes vault edits: every path is resolved strictly inside notes_root.
    "notes_write",
    "notes_create",
    "notes_mkdir",
    "notes_rename",
    "notes_move",
    "notes_copy",
    // Agent's own scratch/plan and the user's own memory/gender: not
    // destructive to the user's files.
    "todowrite",
    "remember",
    "set_user_gender",
    // Objective board edits (project + harness twin): reversible status and
    // step toggles on the user's own goals, not destructive to files.
    "objective_create",
    "objective_update",
    "objective_step_done",
    // Chat bookkeeping that only renames/creates the model's own chats.
    "rename_chat",
    "new_chat",
    // Skill authoring is NOT allowlisted: since Q20 create_skill runs under
    // the Check policy, every agent-initiated skill creation confirms.
    // Attaching a project/vault file to the chat the run is already writing.
    "send_file",
    // Clipboard write hits the general pasteboard (no user file touched).
    "clipboard_set",
    // Pausing/resuming a schedule is a reversible flag toggle; schedule_delete
    // (the destructive one) confirms.
    "schedule_pause",
    // Archiving/unarchiving a project is a reversible visibility toggle: no
    // rows, files or tasks are touched, and the UI mirrors the same action.
    "project_set_archived",
    // Undo pops the bounded undo stack and restores a prior snapshot/trash
    // entry; it only reverses an earlier mutation, never reaches a new path.
    "undo_last",
    // Settings apply (Q.md settings slice): writes one allowlisted config key
    // only through a single-use proposal token; the confirmation is the
    // explicit chat yes the tool description mandates, not the modal queue.
    "settings_apply",
    // Notification Center banner: shows a transient banner, touches no user
    // files; same guard class as clipboard_set.
    "notify",
    // LaunchServices open: launches the default handler for an existing path
    // or an http(s)/file URL; non-destructive, the user closes what opens.
    "open",
    // PDF pages to PNG (Q.md computer-control slice): the source is only
    // read, the PNGs land in the app's own Settings/tmp scratch folder.
    "pdf_to_image",
];

fn is_valid_tool_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn catalogs() -> Vec<(&'static str, Catalog)> {
    vec![
        ("project", project_catalog()),
        ("harness", harness_catalog()),
        ("notes", notes_catalog()),
    ]
}

// Every registered (non-hidden) tool in a catalog, resolved to its Arc<dyn Tool>.
fn all_tools(catalog: &Catalog) -> Vec<(String, std::sync::Arc<dyn super::tool::Tool>)> {
    catalog
        .full_summary()
        .into_iter()
        .map(|(name, _)| match catalog.resolve(&name) {
            Resolution::Found(tool) => (name, tool.clone()),
            _ => panic!("tool {name} vanished between full_summary and resolve"),
        })
        .collect()
}

#[test]
fn tool_names_are_unique() {
    for (label, catalog) in catalogs() {
        let mut names: Vec<String> = catalog.full_summary().into_iter().map(|(n, _)| n).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(before, names.len(), "{label}: duplicate tool names in catalog");
    }
}

#[test]
fn tool_names_match_mcp_safe_pattern() {
    for (label, catalog) in catalogs() {
        for (name, _) in all_tools(&catalog) {
            assert!(
                is_valid_tool_name(&name),
                "{label}: tool name {name} does not match ^[a-z][a-z0-9_]*$"
            );
        }
    }
}

#[test]
fn descriptions_are_non_empty_and_within_cap() {
    for (label, catalog) in catalogs() {
        for (name, tool) in all_tools(&catalog) {
            let desc = tool.description();
            assert!(!desc.trim().is_empty(), "{label}: {name} has an empty description");
            let len = desc.chars().count();
            assert!(
                len <= DESC_CAP,
                "{label}: {name} description is {len} chars (> {DESC_CAP})"
            );
        }
    }
}

#[test]
fn schemas_are_closed_objects_within_size_cap() {
    for (label, catalog) in catalogs() {
        for (name, tool) in all_tools(&catalog) {
            let schema = tool.parameters();
            assert!(schema.is_object(), "{label}: {name} schema is not a JSON object");
            assert_eq!(
                schema["type"], "object",
                "{label}: {name} schema type must be \"object\""
            );
            assert!(
                schema["properties"].is_object(),
                "{label}: {name} schema must declare a properties object"
            );
            assert_eq!(
                schema["additionalProperties"], false,
                "{label}: {name} top-level schema must set additionalProperties:false"
            );
            let serialized = serde_json::to_string(&schema).expect("serialize schema");
            assert!(
                serialized.len() <= SCHEMA_CAP,
                "{label}: {name} schema is {} chars (> {SCHEMA_CAP})",
                serialized.len()
            );
        }
    }
}

#[test]
fn finish_task_is_present_in_every_catalog() {
    for (label, catalog) in catalogs() {
        assert!(
            matches!(catalog.resolve("finish_task"), Resolution::Found(_)),
            "{label}: finish_task is missing from the catalog"
        );
    }
}

#[test]
fn discovery_tools_are_exposed_by_specs() {
    for (label, catalog) in catalogs() {
        let names: Vec<String> = catalog.to_specs().into_iter().map(|s| s.name).collect();
        assert!(
            names.iter().any(|n| n == LIST_TOOLS),
            "{label}: {LIST_TOOLS} not exposed"
        );
        assert!(
            names.iter().any(|n| n == ENABLE_TOOLS),
            "{label}: {ENABLE_TOOLS} not exposed"
        );
    }
}

#[test]
fn mutating_tools_are_guarded_or_allowlisted() {
    let allow: std::collections::BTreeSet<&str> =
        MUTATING_SKIP_ALLOWLIST.iter().copied().collect();
    let mut offenders: Vec<String> = Vec::new();
    for (label, catalog) in catalogs() {
        for (name, tool) in all_tools(&catalog) {
            if tool.class() == ToolClass::Mutating
                && tool.policy() == PermissionPolicy::Skip
                && !allow.contains(name.as_str())
            {
                offenders.push(format!("{label}:{name}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "mutating tools with Skip policy that are not in MUTATING_SKIP_ALLOWLIST \
         (guard them with a confirmation or add them to the reviewed allowlist): {offenders:?}"
    );
}

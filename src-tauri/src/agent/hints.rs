// Actionable hints appended to failed tool results so the model changes
// approach instead of repeating the same failing call (harness reliability).
// Model-facing text is English; the loop appends the result as "\nHint: …".
use super::result::ToolErrorCode;

pub fn hint_for(tool: &str, code: ToolErrorCode, _message: &str) -> Option<String> {
    let hint = match code {
        ToolErrorCode::NotFound if matches!(tool, "read_file" | "fs_read") =>
            "the path may be wrong or missing; call list_tree or fs_list to find the right path",
        ToolErrorCode::UnknownTool =>
            "call list_available_tools to see the tool catalog, then enable_tools to activate the tool you need",
        ToolErrorCode::InvalidArgs =>
            "check the tool's parameter schema and retry with arguments that match it",
        ToolErrorCode::PermissionDenied =>
            "confirm the path is allowed for this project, or pick a path inside the working directory and retry",
        ToolErrorCode::Hidden =>
            "this tool is hidden by policy for this project; choose an allowed alternative",
        _ => return None,
    };
    Some(hint.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_for_read_tools_suggests_the_listing_tools() {
        for tool in ["read_file", "fs_read"] {
            let hint = hint_for(tool, ToolErrorCode::NotFound, "nope not found").unwrap();
            assert!(hint.contains("list_tree"), "{tool}: {hint}");
            assert!(hint.contains("fs_list"), "{tool}: {hint}");
        }
        // not_found from other tools gets no path hint
        assert!(hint_for("task_get", ToolErrorCode::NotFound, "task not found").is_none());
    }

    #[test]
    fn unknown_tool_points_at_the_discovery_meta_tools() {
        let hint = hint_for("nope", ToolErrorCode::UnknownTool, "unknown tool: nope").unwrap();
        assert!(hint.contains("list_available_tools"));
        assert!(hint.contains("enable_tools"));
    }

    #[test]
    fn invalid_args_mentions_the_schema() {
        let hint = hint_for("echo", ToolErrorCode::InvalidArgs, "missing required property").unwrap();
        assert!(hint.contains("schema"));
    }

    #[test]
    fn permission_denied_suggests_fixing_the_path() {
        let hint = hint_for("fs_write", ToolErrorCode::PermissionDenied, "outside allowed roots").unwrap();
        assert!(hint.contains("path"));
    }

    #[test]
    fn hidden_explains_the_policy_block() {
        let hint = hint_for("bash", ToolErrorCode::Hidden, "hidden: disabled").unwrap();
        assert!(hint.contains("policy"));
    }

    #[test]
    fn unmapped_codes_get_no_hint() {
        assert!(hint_for("slow", ToolErrorCode::ExecutionError, "tool timed out").is_none());
        assert!(hint_for("echo", ToolErrorCode::DeniedByUser, "user denied").is_none());
        assert!(hint_for("echo", ToolErrorCode::LimitExceeded, "too big").is_none());
    }
}

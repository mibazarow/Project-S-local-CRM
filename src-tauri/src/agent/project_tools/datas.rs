// Project datas.md tools: datas_add (add/replace a "## <title>" access-notes
// section) and datas_read (whole file). The section parser/body helpers live in
// task_glue and are reused by the harness CRM tools.
use super::support::{fail, require_project, require_wd, with_conn};
use super::super::result::{ToolErrorCode, ToolResult};
use super::super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::agent::task_glue::{datas_add_body, datas_title_arg, read_datas};
use crate::error::Result;
use crate::paths;
use serde_json::{json, Value};

pub struct DatasAddTool;

impl Tool for DatasAddTool {
    fn name(&self) -> &str {
        "datas_add"
    }
    fn description(&self) -> &str {
        "Add or update a \"## <title>\" section in the project's datas.md (access notes); an existing section is replaced, otherwise appended."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "body": { "type": "string" }
            },
            "required": ["title", "body"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let title = match datas_title_arg(args) {
            Ok(title) => title,
            Err(msg) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg)),
        };
        let body = args["body"].as_str().unwrap_or_default();
        let r = with_conn(ctx, |conn| datas_add_body(conn, &wd, pid, &title, body));
        Ok(match r {
            Ok((updated, entry)) => ToolResult::ok(json!({ "title": title, "updated": updated, "entry": entry })),
            Err(e) => fail(e),
        })
    }
}

pub struct DatasReadTool;

impl Tool for DatasReadTool {
    fn name(&self) -> &str {
        "datas_read"
    }
    fn description(&self) -> &str {
        "Read the project's datas.md (access notes file); empty string when the file is missing."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [], "additionalProperties": false })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let r = with_conn(ctx, |conn| read_datas(conn, &wd, pid));
        Ok(match r {
            Ok(text) => {
                ToolResult::ok(json!({ "relPath": paths::DATAS_MD, "content": text })).capped(self.max_result_len())
            }
            Err(e) => fail(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_harness::{exec, setup};
    use serde_json::json;
    use std::fs;

    #[test]
    fn datas_add_updates_sections_and_read_returns_them() {
        let env = setup("pt-datas");
        let r = exec(
            "datas_add",
            json!({ "title": "FTP example.ru", "body": "login: user-1\npass: s3cret" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["updated"], false);

        let r = exec("datas_read", json!({}), &env.ctx);
        let content = r.data.unwrap()["content"].as_str().unwrap().to_owned();
        assert!(content.contains("## FTP example.ru"));
        assert!(content.contains("user-1"));

        // same title updates in place instead of duplicating
        let r = exec(
            "datas_add",
            json!({ "title": "FTP example.ru", "body": "login: user-2" }),
            &env.ctx,
        );
        assert_eq!(r.data.unwrap()["updated"], true);
        let r = exec("datas_read", json!({}), &env.ctx);
        let content = r.data.unwrap()["content"].as_str().unwrap().to_owned();
        assert_eq!(content.matches("## FTP example.ru").count(), 1);
        assert!(content.contains("user-2") && !content.contains("user-1"));

        // a second title appends a second section
        exec(
            "datas_add",
            json!({ "title": "Admin panel", "body": "https://admin.example.ru" }),
            &env.ctx,
        );
        let r = exec("datas_read", json!({}), &env.ctx);
        let content = r.data.unwrap()["content"].as_str().unwrap().to_owned();
        assert!(content.contains("## FTP example.ru") && content.contains("## Admin panel"));
        assert_eq!(content.matches("## ").count(), 2);
        fs::remove_dir_all(&env.wd).unwrap();
    }
}

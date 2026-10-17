use crate::error::{AppError, Result};
use crate::paths;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigKind {
    Llm,
    App,
    Indexing,
    Mcp,
    Ssh,
}

impl ConfigKind {
    pub fn all() -> [ConfigKind; 5] {
        [
            ConfigKind::Llm,
            ConfigKind::App,
            ConfigKind::Indexing,
            ConfigKind::Mcp,
            ConfigKind::Ssh,
        ]
    }

    pub fn filename(self) -> &'static str {
        match self {
            ConfigKind::Llm => paths::CONFIG_LLM,
            ConfigKind::App => paths::CONFIG_APP,
            ConfigKind::Indexing => paths::CONFIG_INDEXING,
            ConfigKind::Mcp => paths::CONFIG_MCP,
            ConfigKind::Ssh => paths::CONFIG_SSH,
        }
    }

    pub fn which(self) -> &'static str {
        match self {
            ConfigKind::Llm => "llm",
            ConfigKind::App => "app",
            ConfigKind::Indexing => "indexing",
            ConfigKind::Mcp => "mcp",
            ConfigKind::Ssh => "ssh",
        }
    }

    pub fn from_filename(name: &str) -> Option<ConfigKind> {
        ConfigKind::all().into_iter().find(|k| k.filename() == name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub context_window: u64,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    // Manual vision override: null = auto-detect via probe, true/false = force.
    pub vision: Option<bool>,
    // Cached result of the last vision probe (informational, written by detect_vision).
    pub vision_detected: Option<bool>,
}

impl LlmConfig {
    // Effective vision support for building model turns: the manual override
    // wins, otherwise the cached probe result; unknown means "do not send
    // images" (the UI warns the user via get_vision_status).
    pub fn vision_enabled(&self) -> bool {
        self.vision
            .unwrap_or_else(|| self.vision_detected.unwrap_or(false))
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: "gpt-4o-mini".into(),
            context_window: 100_000,
            temperature: None,
            max_tokens: None,
            vision: None,
            vision_detected: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    // Agent identity is fixed (name "Project-S", informal «ты»): it is not a
    // setting anymore; old app.json files may still carry agentName /
    // userAddress keys, serde ignores unknown keys so they load unchanged.
    // Full autopilot (SPEC 5.2): every tool confirmation is auto-allowed.
    pub autopilot: bool,
    // Context compaction trigger as % of the model window (SPEC 4.6).
    pub compaction_threshold_percent: u8,
    // Harness session auto-delete threshold in days (SPEC 5.1); 0 disables it.
    pub session_ttl_days: u32,
    // Harness/tmp auto-purge age in days (SPEC 5.2); 0 disables it.
    pub tmp_retention_days: u32,
    // Run journal retention in days (SPEC 3.3); 0 disables pruning.
    pub log_retention_days: u32,
    // "Start with the system" mirror for the UI (SPEC 5.3); the OS
    // LaunchAgent is the source of truth. Default off (SPEC 8).
    pub autostart: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            autopilot: false,
            compaction_threshold_percent: 80,
            session_ttl_days: 30,
            tmp_retention_days: 7,
            log_retention_days: 14,
            autostart: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IndexingConfig {
    pub max_file_size_bytes: u64,
    pub max_text_chars: u64,
    pub exclude_dirs: Vec<String>,
    // The old `embedding` object (enabled/backend/ollamaModel/dim/nameWeight)
    // is gone: semantic search is always on with the in-app ORT model, so it
    // is not configurable. Old indexing.json files still carrying that key
    // load unchanged (serde ignores unknown keys).
}

impl Default for IndexingConfig {
    fn default() -> Self {
        IndexingConfig {
            max_file_size_bytes: 20 * 1024 * 1024,
            max_text_chars: 200_000,
            exclude_dirs: vec![
                ".git".into(),
                "node_modules".into(),
                ".cache".into(),
                "__pycache__".into(),
                ".venv".into(),
                "target".into(),
                "dist".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
    // Unknown top-level keys survive app rewrites (SPEC 3.4 hand-editable configs).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    pub transport: McpTransportConfig,
    // Unknown per-server keys (allowed_tools, cwd, ...) survive app rewrites.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// SPEC 6.2: stdio (command + args + env) or http/sse (url + headers).
// The http variant speaks the streamable HTTP transport (mcp::http), the
// sse variant the legacy HTTP+SSE one (mcp::sse).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpTransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    Sse {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

pub fn file_path(wd: &Path, kind: ConfigKind) -> std::path::PathBuf {
    paths::config_dir(wd).join(kind.filename())
}

fn default_value(kind: ConfigKind) -> Result<Value> {
    let value = match kind {
        ConfigKind::Llm => serde_json::to_value(LlmConfig::default())?,
        ConfigKind::App => serde_json::to_value(AppConfig::default())?,
        ConfigKind::Indexing => serde_json::to_value(IndexingConfig::default())?,
        ConfigKind::Mcp => serde_json::to_value(McpConfig::default())?,
        ConfigKind::Ssh => json!({ "profiles": [] }),
    };
    Ok(value)
}

// Writes default files for configs that don't exist yet.
pub fn ensure_defaults(wd: &Path) -> Result<()> {
    for kind in ConfigKind::all() {
        let file = file_path(wd, kind);
        if !file.exists() {
            write_json(&file, &default_value(kind)?)?;
        }
    }
    Ok(())
}

pub fn read_config(wd: &Path, kind: ConfigKind) -> Result<Value> {
    let file = file_path(wd, kind);
    match fs::read_to_string(&file) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => default_value(kind),
        Err(e) => Err(e.into()),
    }
}

// Merges top-level keys of `value` into the existing file so hand-added
// unknown keys survive app writes.
pub fn write_config(wd: &Path, kind: ConfigKind, value: Value) -> Result<()> {
    let Value::Object(new) = value else {
        return Err(AppError::Invalid("config must be a JSON object".into()));
    };
    let file = file_path(wd, kind);
    let mut map = match fs::read_to_string(&file) {
        Ok(text) => match serde_json::from_str::<Value>(&text)? {
            Value::Object(map) => map,
            _ => Map::new(),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(e.into()),
    };
    map.extend(new);
    write_json(&file, &Value::Object(map))
}

fn write_json(file: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(file, serde_json::to_string_pretty(value)? + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;
    use serde_json::json;

    #[test]
    fn write_config_preserves_unknown_and_hand_edited_keys() {
        let dir = temp_test_dir("config");
        ensure_defaults(&dir).unwrap();

        fs::write(
            file_path(&dir, ConfigKind::Llm),
            r#"{ "model": "hand", "contextWindow": 50000, "customFlag": true }"#,
        )
        .unwrap();

        write_config(&dir, ConfigKind::Llm, json!({ "model": "app", "apiKey": "k" })).unwrap();
        let v = read_config(&dir, ConfigKind::Llm).unwrap();

        assert_eq!(v["model"], "app");
        assert_eq!(v["apiKey"], "k");
        assert_eq!(v["customFlag"], true);
        assert_eq!(v["contextWindow"], 50000);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn defaults_have_expected_shape() {
        let dir = temp_test_dir("config-defaults");
        ensure_defaults(&dir).unwrap();

        assert_eq!(read_config(&dir, ConfigKind::Llm).unwrap()["contextWindow"], 100_000);
        assert_eq!(read_config(&dir, ConfigKind::Mcp).unwrap()["servers"], json!([]));
        let app = read_config(&dir, ConfigKind::App).unwrap();
        assert_eq!(app["autopilot"], false);
        assert!(read_config(&dir, ConfigKind::Indexing)
            .unwrap()
            .get("maxFileSizeBytes")
            .is_some());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn app_config_ignores_removed_identity_keys() {
        // Old app.json files still carry agentName/userAddress: unknown keys
        // are ignored, so existing configs load without a migration.
        let dir = temp_test_dir("config-app-legacy");
        fs::create_dir_all(paths::config_dir(&dir)).unwrap();
        fs::write(
            file_path(&dir, ConfigKind::App),
            r#"{ "agentName": "Вася", "userAddress": "на Вы", "autopilot": true, "sessionTtlDays": 5 }"#,
        )
        .unwrap();
        let cfg: AppConfig =
            serde_json::from_value(read_config(&dir, ConfigKind::App).unwrap()).unwrap();
        assert!(cfg.autopilot);
        assert_eq!(cfg.session_ttl_days, 5);
        // absent keys still fall back to defaults
        assert_eq!(cfg.compaction_threshold_percent, 80);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn indexing_config_ignores_removed_embedding_keys() {
        // Old indexing.json files still carry the embedding object: unknown
        // keys are ignored, so existing configs load without a migration.
        let dir = temp_test_dir("config-indexing-legacy");
        fs::create_dir_all(paths::config_dir(&dir)).unwrap();
        fs::write(
            file_path(&dir, ConfigKind::Indexing),
            r#"{ "maxFileSizeBytes": 1024, "maxTextChars": 500, "excludeDirs": [".git"],
                 "embedding": { "enabled": false, "backend": "ollama", "ollamaModel": "nomic-embed-text", "dim": 512, "nameWeight": 0.5 } }"#,
        )
        .unwrap();
        let cfg: IndexingConfig =
            serde_json::from_value(read_config(&dir, ConfigKind::Indexing).unwrap()).unwrap();
        assert_eq!(cfg.max_file_size_bytes, 1024);
        assert_eq!(cfg.max_text_chars, 500);
        assert_eq!(cfg.exclude_dirs, vec![".git".to_string()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_missing_config_returns_default_without_creating_file() {
        let dir = temp_test_dir("config-missing");
        let v = read_config(&dir, ConfigKind::Mcp).unwrap();
        assert_eq!(v, json!({ "servers": [] }));
        assert!(!file_path(&dir, ConfigKind::Mcp).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn llm_vision_fields_round_trip_and_default_to_auto() {
        let cfg: LlmConfig = serde_json::from_value(json!({
            "baseUrl": "b", "model": "m", "contextWindow": 1,
            "vision": false, "visionDetected": true,
        }))
        .unwrap();
        assert_eq!(cfg.vision, Some(false));
        assert_eq!(cfg.vision_detected, Some(true));
        // Manual override wins over the cached probe result.
        assert!(!cfg.vision_enabled());

        let cached: LlmConfig =
            serde_json::from_value(json!({ "baseUrl": "b", "model": "m", "contextWindow": 1, "visionDetected": true }))
                .unwrap();
        assert!(cached.vision_enabled());

        let auto: LlmConfig = serde_json::from_value(json!({ "baseUrl": "b", "model": "m", "contextWindow": 1 }))
            .unwrap();
        assert_eq!((auto.vision, auto.vision_detected), (None, None));
        assert!(!auto.vision_enabled());
    }

    #[test]
    fn mcp_transport_variants_parse() {
        let cfg: McpConfig = serde_json::from_value(json!({ "servers": [
            { "name": "std", "enabled": true, "transport": { "type": "stdio", "command": "sh", "args": ["-c", "x"], "env": { "K": "V" } } },
            { "name": "h", "enabled": true, "transport": { "type": "http", "url": "https://x.example/mcp", "headers": { "A": "b" } } },
            { "name": "s", "enabled": true, "transport": { "type": "sse", "url": "http://localhost:1/sse" } },
        ] }))
        .unwrap();
        assert_eq!(cfg.servers.len(), 3);
        assert!(matches!(&cfg.servers[0].transport, McpTransportConfig::Stdio { command, args, env }
            if command == "sh" && args.len() == 2 && env["K"] == "V"));
        assert!(matches!(&cfg.servers[1].transport, McpTransportConfig::Http { url, headers }
            if url == "https://x.example/mcp" && headers["A"] == "b"));
        assert!(matches!(&cfg.servers[2].transport, McpTransportConfig::Sse { .. }));
    }

    #[test]
    fn mcp_config_keeps_unknown_keys_through_app_rewrites() {
        let dir = temp_test_dir("config-mcp-unknown");
        ensure_defaults(&dir).unwrap();
        write_config(
            &dir,
            ConfigKind::Mcp,
            json!({ "servers": [{
                "name": "srv",
                "enabled": true,
                "transport": { "type": "stdio", "command": "sh", "args": [] },
                "handAdded": 7
            }], "futureTop": true }),
        )
        .unwrap();
        let typed: McpConfig = serde_json::from_value(read_config(&dir, ConfigKind::Mcp).unwrap()).unwrap();
        write_config(&dir, ConfigKind::Mcp, serde_json::to_value(&typed).unwrap()).unwrap();
        let raw = read_config(&dir, ConfigKind::Mcp).unwrap();
        assert_eq!(raw["servers"][0]["handAdded"], 7);
        assert_eq!(raw["futureTop"], true);
        fs::remove_dir_all(&dir).unwrap();
    }
}

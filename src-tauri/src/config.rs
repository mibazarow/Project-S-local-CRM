use crate::error::{AppError, Result};
use crate::paths;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
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
}

impl ConfigKind {
    pub fn all() -> [ConfigKind; 4] {
        [ConfigKind::Llm, ConfigKind::App, ConfigKind::Indexing, ConfigKind::Mcp]
    }

    pub fn filename(self) -> &'static str {
        match self {
            ConfigKind::Llm => paths::CONFIG_LLM,
            ConfigKind::App => paths::CONFIG_APP,
            ConfigKind::Indexing => paths::CONFIG_INDEXING,
            ConfigKind::Mcp => paths::CONFIG_MCP,
        }
    }

    pub fn which(self) -> &'static str {
        match self {
            ConfigKind::Llm => "llm",
            ConfigKind::App => "app",
            ConfigKind::Indexing => "indexing",
            ConfigKind::Mcp => "mcp",
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    // Agent identity (SPEC 6.3): mirrored into memory records on every
    // context build, so settings edits show up in the next session.
    pub agent_name: String,
    pub user_address: String,
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
            agent_name: "Агент".into(),
            user_address: "на ты".into(),
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
pub struct EmbeddingConfig {
    pub enabled: bool,
    // "noop" | "ollama" | "ort"; unknown values fall back to noop at runtime.
    pub backend: String,
    pub ollama_model: String,
    pub dim: usize,
    pub name_weight: f32,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        EmbeddingConfig {
            enabled: true,
            // Noop by default so the app never probes the network unasked;
            // switch to "ollama"/"ort" in indexing.json or the settings UI.
            backend: "noop".into(),
            ollama_model: "nomic-embed-text".into(),
            dim: crate::embed::EMBED_DIM,
            name_weight: 0.3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IndexingConfig {
    pub max_file_size_bytes: u64,
    pub max_text_chars: u64,
    pub exclude_dirs: Vec<String>,
    pub embedding: EmbeddingConfig,
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
            embedding: EmbeddingConfig::default(),
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
// The http/sse variants parse now; their transport lands in a later slice.
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
        assert_eq!(app["agentName"], "Агент");
        assert_eq!(app["userAddress"], "на ты");
        assert!(read_config(&dir, ConfigKind::Indexing)
            .unwrap()
            .get("maxFileSizeBytes")
            .is_some());

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

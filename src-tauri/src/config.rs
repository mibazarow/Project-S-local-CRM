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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LlmProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub context_window: u64,
    // Per-model context windows chosen in Settings, keyed by model id. The
    // global `context_window` above is the fallback for models with no entry.
    pub model_context_windows: BTreeMap<String, u64>,
    // Maximum context window reported by the provider for a model, keyed by
    // model id. Absent means the provider did not report one.
    pub model_context_max: BTreeMap<String, u64>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    // Manual vision override: null = auto-detect via probe, true/false = force.
    pub vision: Option<bool>,
    // Cached result of the last vision probe (informational, written by detect_vision).
    pub vision_detected: Option<bool>,
    // Thinking mode level: "off" | "low" | "medium" | "high". Off omits
    // `reasoning_effort` from requests; other levels send it as-is.
    #[serde(default = "default_thinking_level")]
    pub thinking_level: String,
    // Saved provider profiles (Q-24): any number, exactly one active. The
    // top-level base_url/api_key mirror the active provider so existing
    // readers keep working; they are the fallback when no provider is active.
    pub providers: Vec<LlmProvider>,
    pub active_provider: Option<String>,
}

// Stable-enough id for a local single-user config: process-unique counter
// combined with wall-clock millis, no extra dependency.
pub fn new_provider_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("p{millis}-{n}")
}

pub fn default_thinking_level() -> String {
    "medium".into()
}

// Used in Settings when the provider does not report a context maximum, and
// as the upper bound the user may raise an unreported window to.
pub const DEFAULT_MODEL_CONTEXT_WINDOW: u64 = 100_000;
pub const MAX_UNREPORTED_CONTEXT_WINDOW: u64 = 250_000;

impl LlmConfig {
    // Provider-reported maximum for a model, when known.
    pub fn reported_max_context(&self, model: &str) -> Option<u64> {
        self.model_context_max.get(model).copied().filter(|v| *v > 0)
    }

    // Effective context window for the active model: its per-model entry when
    // present, otherwise the legacy global `contextWindow` fallback.
    pub fn effective_context_window(&self, model: &str) -> u64 {
        let window = self
            .model_context_windows
            .get(model)
            .copied()
            .filter(|v| *v > 0)
            .unwrap_or(self.context_window);
        self.clamp_context_window(model, window)
    }

    // Backend clamp: never above the reported maximum, and never above 250k
    // when the provider did not report one; always at least one token.
    pub fn clamp_context_window(&self, model: &str, window: u64) -> u64 {
        let max = self
            .reported_max_context(model)
            .unwrap_or(MAX_UNREPORTED_CONTEXT_WINDOW);
        window.clamp(1, max.max(1))
    }

    // Effective vision support for building model turns: the manual override
    // wins, otherwise the cached probe result; unknown means "do not send
    // images" (the UI warns the user via get_vision_status).
    pub fn vision_enabled(&self) -> bool {
        self.vision
            .unwrap_or_else(|| self.vision_detected.unwrap_or(false))
    }

    // Request mapping for the thinking level: "off" omits reasoning_effort,
    // an unrecognized or empty value (older llm.json files) sends the shipped
    // default "medium".
    pub fn reasoning_effort(&self) -> Option<&str> {
        match self.thinking_level.as_str() {
            "off" => None,
            "low" => Some("low"),
            "high" => Some("high"),
            _ => Some("medium"),
        }
    }

    // The active provider profile, when one is selected and still present.
    pub fn active(&self) -> Option<&LlmProvider> {
        let id = self.active_provider.as_deref()?;
        self.providers.iter().find(|p| p.id == id)
    }

    // Effective endpoint/key: the active provider when present, otherwise the
    // legacy top-level fields (backward compatible with pre-Q-24 llm.json).
    pub fn effective_base_url(&self) -> &str {
        self.active()
            .map(|p| p.base_url.as_str())
            .unwrap_or(&self.base_url)
    }

    pub fn effective_api_key(&self) -> &str {
        self.active()
            .map(|p| p.api_key.as_str())
            .unwrap_or(&self.api_key)
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: "gpt-4o-mini".into(),
            context_window: DEFAULT_MODEL_CONTEXT_WINDOW,
            model_context_windows: BTreeMap::new(),
            model_context_max: BTreeMap::new(),
            temperature: None,
            max_tokens: None,
            vision: None,
            vision_detected: None,
            thinking_level: default_thinking_level(),
            providers: Vec::new(),
            active_provider: None,
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
    // Automatic backup interval in days (SPEC 3.6); 0 disables auto-backup.
    pub backup_interval_days: u32,
    // How many newest backups auto-backup retention keeps (SPEC 3.6).
    pub backup_keep: u32,
    // Whole-UI scale as a percentage (Q.md item 4). Only the six shipped steps
    // are valid; read_config normalizes anything else to 100.
    pub ui_scale_percent: u32,
}

// The only UI scale steps the settings control offers. Anything else on disk is
// treated as the default 100 on read.
pub const UI_SCALE_STEPS: [u32; 4] = [100, 110, 130, 175];
pub const DEFAULT_UI_SCALE_PERCENT: u32 = 100;

pub fn normalize_ui_scale(value: u64) -> u32 {
    UI_SCALE_STEPS
        .iter()
        .find(|&&step| step as u64 == value)
        .copied()
        .unwrap_or(DEFAULT_UI_SCALE_PERCENT)
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
            backup_interval_days: 1,
            backup_keep: 7,
            ui_scale_percent: DEFAULT_UI_SCALE_PERCENT,
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

impl IndexingConfig {
    // Current shipped defaults (10 MB / 1 000 000 chars).
    pub const DEFAULT_MAX_FILE_SIZE_BYTES: u64 = 10 * 1024 * 1024;
    pub const DEFAULT_MAX_TEXT_CHARS: u64 = 1_000_000;
}

impl Default for IndexingConfig {
    fn default() -> Self {
        IndexingConfig {
            max_file_size_bytes: Self::DEFAULT_MAX_FILE_SIZE_BYTES,
            max_text_chars: Self::DEFAULT_MAX_TEXT_CHARS,
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
    // Per-server tool-call timeout in seconds; falls back to the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
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
    migrate_indexing_defaults(wd)?;
    migrate_llm_providers(wd)
}

// Limits that shipped as indexing.json defaults before the 10 MB / 1M chars
// change. An existing file still holding exactly these values was never
// customized, so it is bumped to the new defaults once (see below).
const LEGACY_MAX_FILE_SIZE_BYTES: u64 = 20 * 1024 * 1024;
const LEGACY_MAX_TEXT_CHARS: u64 = 200_000;

// One-time migration of indexing.json limits: a file that still carries the
// old shipped defaults (20 MB / 200 000) is bumped to the new defaults
// (10 MB / 1 000 000). User-customized values are left untouched, and the
// file is rewritten only when at least one field actually changed.
pub fn migrate_indexing_defaults(wd: &Path) -> Result<()> {
    let file = file_path(wd, ConfigKind::Indexing);
    let Ok(text) = fs::read_to_string(&file) else {
        return Ok(()); // no file yet: ensure_defaults already writes new ones
    };
    let mut value: Value = serde_json::from_str(&text)?;
    let Some(obj) = value.as_object_mut() else {
        return Ok(());
    };
    let mut changed = false;
    if obj.get("maxFileSizeBytes").and_then(Value::as_u64) == Some(LEGACY_MAX_FILE_SIZE_BYTES) {
        obj.insert(
            "maxFileSizeBytes".into(),
            json!(IndexingConfig::DEFAULT_MAX_FILE_SIZE_BYTES),
        );
        changed = true;
    }
    if obj.get("maxTextChars").and_then(Value::as_u64) == Some(LEGACY_MAX_TEXT_CHARS) {
        obj.insert(
            "maxTextChars".into(),
            json!(IndexingConfig::DEFAULT_MAX_TEXT_CHARS),
        );
        changed = true;
    }
    if changed {
        write_json(&file, &value)?;
    }
    Ok(())
}

// One-time migration of llm.json to the multi-provider shape (Q-24): a file
// with no `providers` but a non-empty legacy `baseUrl` gets a single
// «По умолчанию» provider seeded from the legacy endpoint/key and marked
// active. A file that already has providers but no active id (e.g. a restored
// backup) gets its first provider activated. The legacy top-level baseUrl/apiKey
// are left as-is (they already mirror the seeded provider). Idempotent.
pub fn migrate_llm_providers(wd: &Path) -> Result<()> {
    let file = file_path(wd, ConfigKind::Llm);
    let Ok(text) = fs::read_to_string(&file) else {
        return Ok(()); // no file yet: ensure_defaults already wrote the default
    };
    let mut value: Value = serde_json::from_str(&text)?;
    let Some(obj) = value.as_object_mut() else {
        return Ok(());
    };
    let providers = obj
        .get("providers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if providers.is_empty() {
        let base_url = obj.get("baseUrl").and_then(Value::as_str).unwrap_or("");
        if base_url.trim().is_empty() {
            return Ok(()); // nothing to seed
        }
        let api_key = obj.get("apiKey").and_then(Value::as_str).unwrap_or("");
        let id = new_provider_id();
        obj.insert(
            "providers".into(),
            json!([{ "id": id, "name": "По умолчанию", "baseUrl": base_url, "apiKey": api_key }]),
        );
        obj.insert("activeProvider".into(), json!(id));
        write_json(&file, &value)?;
        return Ok(());
    }
    // Providers exist but none is active: activate the first and mirror it.
    if obj.get("activeProvider").and_then(Value::as_str).is_none() {
        if let Some(first) = providers.first().and_then(Value::as_object) {
            let id = first.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            if !id.is_empty() {
                obj.insert("activeProvider".into(), json!(id));
                if let Some(bu) = first.get("baseUrl") {
                    obj.insert("baseUrl".into(), bu.clone());
                }
                if let Some(ak) = first.get("apiKey") {
                    obj.insert("apiKey".into(), ak.clone());
                }
                write_json(&file, &value)?;
            }
        }
    }
    Ok(())
}

pub fn read_config(wd: &Path, kind: ConfigKind) -> Result<Value> {
    let file = file_path(wd, kind);
    let mut value = match fs::read_to_string(&file) {
        Ok(text) => serde_json::from_str(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => default_value(kind)?,
        Err(e) => return Err(e.into()),
    };
    if kind == ConfigKind::App {
        normalize_app_ui_scale(&mut value);
    }
    Ok(value)
}

// Clamp app.json's uiScalePercent to a shipped step (100 when it is a number
// outside the allowed set) so the UI never reads a bogus scale. The file on disk
// is left untouched; the settings control writes a valid value back.
fn normalize_app_ui_scale(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if let Some(raw) = obj.get("uiScalePercent").and_then(Value::as_u64) {
        obj.insert("uiScalePercent".into(), json!(normalize_ui_scale(raw)));
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
        assert_eq!(
            read_config(&dir, ConfigKind::Llm).unwrap()["modelContextWindows"],
            json!({})
        );
        assert_eq!(read_config(&dir, ConfigKind::Llm).unwrap()["modelContextMax"], json!({}));
        assert_eq!(read_config(&dir, ConfigKind::Mcp).unwrap()["servers"], json!([]));
        let app = read_config(&dir, ConfigKind::App).unwrap();
        assert_eq!(app["autopilot"], false);
        let indexing = read_config(&dir, ConfigKind::Indexing).unwrap();
        assert_eq!(indexing["maxFileSizeBytes"], 10 * 1024 * 1024);
        assert_eq!(indexing["maxTextChars"], 1_000_000);

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
    fn app_config_ui_scale_defaults_to_100() {
        let dir = temp_test_dir("config-ui-scale-default");
        ensure_defaults(&dir).unwrap();
        let app = read_config(&dir, ConfigKind::App).unwrap();
        assert_eq!(app["uiScalePercent"], 100);
        let typed: AppConfig = serde_json::from_value(app).unwrap();
        assert_eq!(typed.ui_scale_percent, 100);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_config_clamps_invalid_ui_scale_to_100_and_keeps_other_keys() {
        let dir = temp_test_dir("config-ui-scale-clamp");
        fs::create_dir_all(paths::config_dir(&dir)).unwrap();
        fs::write(
            file_path(&dir, ConfigKind::App),
            r#"{ "autopilot": true, "uiScalePercent": 137, "customKey": 5 }"#,
        )
        .unwrap();
        let app = read_config(&dir, ConfigKind::App).unwrap();
        assert_eq!(app["uiScalePercent"], 100);
        // Other keys (known and hand-added) survive the read normalization.
        assert_eq!(app["autopilot"], true);
        assert_eq!(app["customKey"], 5);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_config_keeps_every_valid_ui_scale_step() {
        let dir = temp_test_dir("config-ui-scale-valid");
        fs::create_dir_all(paths::config_dir(&dir)).unwrap();
        for step in UI_SCALE_STEPS {
            fs::write(
                file_path(&dir, ConfigKind::App),
                &format!(r#"{{ "uiScalePercent": {step} }}"#),
            )
            .unwrap();
            let app = read_config(&dir, ConfigKind::App).unwrap();
            assert_eq!(app["uiScalePercent"], step, "step {step}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn normalize_ui_scale_maps_everything_else_to_100() {
        for step in UI_SCALE_STEPS {
            assert_eq!(normalize_ui_scale(step as u64), step);
        }
        for bad in [0u64, 1, 99, 101, 120, 125, 137, 150, 200, 201, 1000, u64::MAX] {
            assert_eq!(normalize_ui_scale(bad), DEFAULT_UI_SCALE_PERCENT, "bad {bad}");
        }
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
    fn indexing_migration_bumps_legacy_defaults_to_new_values() {
        let dir = temp_test_dir("config-indexing-migrate");
        fs::create_dir_all(paths::config_dir(&dir)).unwrap();
        fs::write(
            file_path(&dir, ConfigKind::Indexing),
            r#"{ "maxFileSizeBytes": 20971520, "maxTextChars": 200000, "excludeDirs": [".git"] }"#,
        )
        .unwrap();

        migrate_indexing_defaults(&dir).unwrap();
        let v = read_config(&dir, ConfigKind::Indexing).unwrap();
        assert_eq!(v["maxFileSizeBytes"], 10 * 1024 * 1024);
        assert_eq!(v["maxTextChars"], 1_000_000);
        // untouched fields survive the rewrite
        assert_eq!(v["excludeDirs"], json!([".git"]));

        // Idempotent: a second pass leaves the new values alone.
        migrate_indexing_defaults(&dir).unwrap();
        let v = read_config(&dir, ConfigKind::Indexing).unwrap();
        assert_eq!(v["maxFileSizeBytes"], 10 * 1024 * 1024);
        assert_eq!(v["maxTextChars"], 1_000_000);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn indexing_migration_keeps_user_customized_values() {
        let dir = temp_test_dir("config-indexing-custom");
        fs::create_dir_all(paths::config_dir(&dir)).unwrap();
        // One field customized, the other still the legacy default: only the
        // legacy one moves, the custom one is preserved.
        fs::write(
            file_path(&dir, ConfigKind::Indexing),
            r#"{ "maxFileSizeBytes": 5242880, "maxTextChars": 200000, "excludeDirs": [".git", "vendor"] }"#,
        )
        .unwrap();

        migrate_indexing_defaults(&dir).unwrap();
        let v = read_config(&dir, ConfigKind::Indexing).unwrap();
        assert_eq!(v["maxFileSizeBytes"], 5242880); // custom, kept
        assert_eq!(v["maxTextChars"], 1_000_000); // legacy, migrated
        assert_eq!(v["excludeDirs"], json!([".git", "vendor"]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn indexing_migration_is_a_noop_without_a_file() {
        let dir = temp_test_dir("config-indexing-missing");
        migrate_indexing_defaults(&dir).unwrap();
        assert!(!file_path(&dir, ConfigKind::Indexing).exists());
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
    fn per_model_context_window_wins_and_is_clamped_to_the_reported_max() {
        let cfg: LlmConfig = serde_json::from_value(json!({
            "baseUrl": "b", "model": "m", "contextWindow": 240000,
            "modelContextWindows": { "m": 300000, "other": 50000 },
            "modelContextMax": { "m": 128000 },
        }))
        .unwrap();
        // Per-model entry wins over the global fallback and cannot exceed the
        // provider-reported maximum.
        assert_eq!(cfg.effective_context_window("m"), 128000);
        assert_eq!(cfg.effective_context_window("other"), 50000);
        // No entry and no reported max: legacy global fallback, capped at 250k.
        let fallback: LlmConfig = serde_json::from_value(json!({
            "baseUrl": "b", "model": "x", "contextWindow": 400000,
        }))
        .unwrap();
        assert_eq!(fallback.effective_context_window("x"), 250_000);
        assert_eq!(fallback.effective_context_window("unknown"), 250_000);
        assert_eq!(cfg.clamp_context_window("m", 0), 1);
        let legacy: LlmConfig =
            serde_json::from_value(json!({ "baseUrl": "b", "model": "m", "contextWindow": 65536 }))
                .unwrap();
        assert_eq!(legacy.effective_context_window("m"), 65536);
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
    fn thinking_level_defaults_to_medium_and_maps_to_reasoning_effort() {
        // Shipped default: medium, and a written llm.json carries it.
        assert_eq!(LlmConfig::default().thinking_level, "medium");
        assert_eq!(LlmConfig::default().reasoning_effort(), Some("medium"));

        // An existing llm.json without the key loads as medium (no migration).
        let legacy: LlmConfig =
            serde_json::from_value(json!({ "baseUrl": "b", "model": "m", "contextWindow": 1 }))
                .unwrap();
        assert_eq!(legacy.reasoning_effort(), Some("medium"));

        for (level, effort) in [
            ("off", None),
            ("low", Some("low")),
            ("medium", Some("medium")),
            ("high", Some("high")),
            ("", Some("medium")),
            ("bogus", Some("medium")),
        ] {
            let cfg: LlmConfig = serde_json::from_value(json!({
                "baseUrl": "b", "model": "m", "contextWindow": 1, "thinkingLevel": level,
            }))
            .unwrap();
            assert_eq!(cfg.reasoning_effort(), effort, "level {level:?}");
        }
    }

    #[test]
    fn llm_migration_seeds_one_provider_from_legacy_fields() {
        let dir = temp_test_dir("config-llm-migrate");
        fs::create_dir_all(paths::config_dir(&dir)).unwrap();
        fs::write(
            file_path(&dir, ConfigKind::Llm),
            r#"{ "baseUrl": "https://api.deepseek.com/v1", "apiKey": "sk-x", "model": "deepseek-chat", "contextWindow": 65536 }"#,
        )
        .unwrap();

        migrate_llm_providers(&dir).unwrap();
        let v = read_config(&dir, ConfigKind::Llm).unwrap();
        let providers = v["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["name"], "По умолчанию");
        assert_eq!(providers[0]["baseUrl"], "https://api.deepseek.com/v1");
        assert_eq!(providers[0]["apiKey"], "sk-x");
        assert_eq!(v["activeProvider"], providers[0]["id"]);
        // Legacy fields are untouched (they already mirror the seeded provider).
        assert_eq!(v["baseUrl"], "https://api.deepseek.com/v1");
        assert_eq!(v["apiKey"], "sk-x");
        assert_eq!(v["model"], "deepseek-chat");

        // Idempotent: a second pass does not add another provider.
        migrate_llm_providers(&dir).unwrap();
        let v = read_config(&dir, ConfigKind::Llm).unwrap();
        assert_eq!(v["providers"].as_array().unwrap().len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn llm_migration_is_a_noop_without_a_baseurl_or_file() {
        let dir = temp_test_dir("config-llm-migrate-empty");
        fs::create_dir_all(paths::config_dir(&dir)).unwrap();
        fs::write(
            file_path(&dir, ConfigKind::Llm),
            r#"{ "baseUrl": "", "apiKey": "", "model": "" }"#,
        )
        .unwrap();
        migrate_llm_providers(&dir).unwrap();
        let v = read_config(&dir, ConfigKind::Llm).unwrap();
        assert!(v.get("providers").map(|p| p.as_array().unwrap().is_empty()).unwrap_or(true));
        assert!(v.get("activeProvider").is_none());

        // Missing file: no error, nothing created.
        let dir2 = temp_test_dir("config-llm-migrate-missing");
        migrate_llm_providers(&dir2).unwrap();
        assert!(!file_path(&dir2, ConfigKind::Llm).exists());
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&dir2).unwrap();
    }

    #[test]
    fn llm_migration_activates_first_provider_when_none_active() {
        let dir = temp_test_dir("config-llm-migrate-activate");
        fs::create_dir_all(paths::config_dir(&dir)).unwrap();
        fs::write(
            file_path(&dir, ConfigKind::Llm),
            r#"{ "baseUrl": "", "apiKey": "", "activeProvider": null, "providers": [
                 { "id": "a", "name": "A", "baseUrl": "https://a.test/v1", "apiKey": "ka" },
                 { "id": "b", "name": "B", "baseUrl": "https://b.test/v1", "apiKey": "kb" } ] }"#,
        )
        .unwrap();
        migrate_llm_providers(&dir).unwrap();
        let v = read_config(&dir, ConfigKind::Llm).unwrap();
        assert_eq!(v["activeProvider"], "a");
        // The active provider is mirrored into the legacy fields.
        assert_eq!(v["baseUrl"], "https://a.test/v1");
        assert_eq!(v["apiKey"], "ka");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn llm_resolution_prefers_active_and_falls_back_to_legacy() {
        // Active provider present: effective values come from it.
        let cfg: LlmConfig = serde_json::from_value(json!({
            "baseUrl": "https://legacy.test/v1", "apiKey": "legacy",
            "model": "m", "contextWindow": 1,
            "providers": [
                { "id": "a", "name": "A", "baseUrl": "https://a.test/v1", "apiKey": "ka" },
                { "id": "b", "name": "B", "baseUrl": "https://b.test/v1", "apiKey": "kb" } ],
            "activeProvider": "b",
        }))
        .unwrap();
        assert_eq!(cfg.effective_base_url(), "https://b.test/v1");
        assert_eq!(cfg.effective_api_key(), "kb");

        // No active id (or a dangling id): fall back to the legacy fields.
        let legacy: LlmConfig = serde_json::from_value(json!({
            "baseUrl": "https://legacy.test/v1", "apiKey": "legacy",
            "model": "m", "contextWindow": 1,
            "providers": [{ "id": "a", "name": "A", "baseUrl": "https://a.test/v1", "apiKey": "ka" }],
            "activeProvider": "missing",
        }))
        .unwrap();
        assert_eq!(legacy.effective_base_url(), "https://legacy.test/v1");
        assert_eq!(legacy.effective_api_key(), "legacy");

        // Pre-Q-24 file (no providers at all): legacy fields win.
        let old: LlmConfig = serde_json::from_value(json!({
            "baseUrl": "https://old.test/v1", "apiKey": "old", "model": "m", "contextWindow": 1,
        }))
        .unwrap();
        assert_eq!(old.effective_base_url(), "https://old.test/v1");
        assert_eq!(old.effective_api_key(), "old");
    }

    #[test]
    fn mcp_transport_variants_parse() {
        let cfg: McpConfig = serde_json::from_value(json!({ "servers": [
            { "name": "std", "enabled": true, "timeoutSecs": 30, "transport": { "type": "stdio", "command": "sh", "args": ["-c", "x"], "env": { "K": "V" } } },
            { "name": "h", "enabled": true, "transport": { "type": "http", "url": "https://x.example/mcp", "headers": { "A": "b" } } },
            { "name": "s", "enabled": true, "transport": { "type": "sse", "url": "http://localhost:1/sse" } },
        ] }))
        .unwrap();
        assert_eq!(cfg.servers.len(), 3);
        assert_eq!(cfg.servers[0].timeout_secs, Some(30));
        assert_eq!(cfg.servers[1].timeout_secs, None);
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

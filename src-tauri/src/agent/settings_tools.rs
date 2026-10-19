// Settings registry and the safe chat edit tools (Q.md "настройки из чата"):
// the agent sees the REAL keys/values from Settings/config/*.json, proposes a
// validated change, and applies it only after an explicit confirmation in the
// chat text — no modal: all three tools run under the Skip policy, the chat
// question IS the confirmation. Writes go through the same
// config::read_config/write_config path the settings UI uses, so unknown keys
// survive and the config file watcher emits config-changed exactly like after
// mcp_add. Secrets never leave the registry: key fields are shown only as
// set/empty, provider URLs only as the host.
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::config::{self, ConfigKind};
use crate::error::{AppError, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// A proposal token is valid for five minutes and consumed by the first apply.
const PROPOSAL_TTL: Duration = Duration::from_secs(300);

struct Def {
    key: &'static str,
    kind: ConfigKind,
    label: &'static str,
    description: &'static str,
    vtype: &'static str,
    allowed: Value,
    default: Value,
    writable: bool,
    deny: &'static str,
}

// The reviewed allow/deny list (Q.md settings slice). Anything not listed
// here and not a registry key is refused outright: secrets, provider wiring,
// paths, MCP/SSH and data-deletion keys stay UI-only.
fn defs() -> Vec<Def> {
    vec![
        Def {
            key: "llm.model",
            kind: ConfigKind::Llm,
            label: "Модель ИИ",
            description: "Название модели, которой агент отвечает (например gpt-4o-mini). Влияет на качество, скорость и цену ответов.",
            vtype: "string",
            allowed: json!("любая непустая строка"),
            default: json!("gpt-4o-mini"),
            writable: true,
            deny: "",
        },
        Def {
            key: "llm.thinkingLevel",
            kind: ConfigKind::Llm,
            label: "Уровень размышлений",
            description: "Насколько глубоко модель думает перед ответом: off, low, medium или high. Выше — точнее на сложных задачах, но медленнее и дороже.",
            vtype: "enum",
            allowed: json!(["off", "low", "medium", "high"]),
            default: json!("medium"),
            writable: true,
            deny: "",
        },
        Def {
            key: "llm.contextWindow",
            kind: ConfigKind::Llm,
            label: "Размер контекста",
            description: "Сколько токенов помещается в контекст модели. Больше — длиннее переписка умещается без сжатия. Зависит от модели.",
            vtype: "uint",
            allowed: json!({"min": 1}),
            default: json!(100_000),
            writable: true,
            deny: "",
        },
        Def {
            key: "llm.maxTokens",
            kind: ConfigKind::Llm,
            label: "Лимит ответа",
            description: "Максимум токенов в одном ответе модели; null — без лимита.",
            vtype: "uint|null",
            allowed: json!({"min": 1, "or": "null"}),
            default: Value::Null,
            writable: true,
            deny: "",
        },
        Def {
            key: "llm.vision",
            kind: ConfigKind::Llm,
            label: "Поддержка изображений",
            description: "Видит ли модель картинки: auto (определять автоматически), yes (принудительно да), no (нет).",
            vtype: "auto|yes|no",
            allowed: json!(["auto", "yes", "no"]),
            default: json!("auto"),
            writable: true,
            deny: "",
        },
        Def {
            key: "indexing.maxFileSizeBytes",
            kind: ConfigKind::Indexing,
            label: "Максимальный размер файла",
            description: "Максимальный размер файла в байтах, который попадает в индекс поиска. Файлы больше индексируются частично или пропускаются.",
            vtype: "uint (байты)",
            allowed: json!({"min": 1}),
            default: json!(10 * 1024 * 1024),
            writable: true,
            deny: "",
        },
        Def {
            key: "indexing.maxTextChars",
            kind: ConfigKind::Indexing,
            label: "Максимум символов из файла",
            description: "Сколько символов текста из одного файла попадает в индекс. Больше — глубже поиск по длинным документам, но медленнее индексация.",
            vtype: "uint (символы)",
            allowed: json!({"min": 1}),
            default: json!(1_000_000),
            writable: true,
            deny: "",
        },
        Def {
            key: "indexing.excludeDirs",
            kind: ConfigKind::Indexing,
            label: "Исключаемые папки",
            description: "Имена папок, которые не индексируются (например node_modules, .git). Можно прислать массив имён или строку через запятую.",
            vtype: "string[]",
            allowed: json!("массив строк или строка через запятую"),
            default: json!([".git", "node_modules", ".cache", "__pycache__", ".venv", "target", "dist"]),
            writable: true,
            deny: "",
        },
        Def {
            key: "app.backupIntervalDays",
            kind: ConfigKind::App,
            label: "Интервал автобэкапа",
            description: "Как часто делать автоматические резервные копии, в днях; 0 — автобэкап выключен.",
            vtype: "uint (дни)",
            allowed: json!({"min": 0, "0": "выключено"}),
            default: json!(1),
            writable: true,
            deny: "",
        },
        Def {
            key: "app.backupKeep",
            kind: ConfigKind::App,
            label: "Хранить бэкапов",
            description: "Сколько последних резервных копий хранить; лишние автобэкап удаляет.",
            vtype: "uint",
            allowed: json!({"min": 0}),
            default: json!(7),
            writable: true,
            deny: "",
        },
        Def {
            key: "app.compactionThresholdPercent",
            kind: ConfigKind::App,
            label: "Порог сжатия контекста",
            description: "При каком проценте заполнения контекста сжимать историю диалога (10–95). Ниже — чаще сжатие, выше — риск переполнения окна.",
            vtype: "uint (процент)",
            allowed: json!({"min": 10, "max": 95}),
            default: json!(80),
            writable: true,
            deny: "",
        },
        Def {
            key: "llm.apiKey",
            kind: ConfigKind::Llm,
            label: "API-ключ",
            description: "Наличие секретного ключа провайдера: только set или empty, само значение никогда не показывается.",
            vtype: "secret",
            allowed: Value::Null,
            default: json!("empty"),
            writable: false,
            deny: "секретный ключ: значение скрыто, меняется только в UI настроек",
        },
        Def {
            key: "llm.providerName",
            kind: ConfigKind::Llm,
            label: "Активный провайдер",
            description: "Название активного профиля провайдера (без адреса и ключа).",
            vtype: "string",
            allowed: Value::Null,
            default: json!("—"),
            writable: false,
            deny: "провайдеры и их доступы меняются только в UI настроек",
        },
        Def {
            key: "llm.providerHost",
            kind: ConfigKind::Llm,
            label: "Хост провайдера",
            description: "Хост активного адреса API (полный адрес не показывается).",
            vtype: "string",
            allowed: Value::Null,
            default: json!("—"),
            writable: false,
            deny: "адрес провайдера меняется только в UI настроек",
        },
        Def {
            key: "llm.temperature",
            kind: ConfigKind::Llm,
            label: "Температура",
            description: "Температура фиксирована для агента и игнорируется при запросах.",
            vtype: "number",
            allowed: Value::Null,
            default: Value::Null,
            writable: false,
            deny: "температура фиксирована для агента и игнорируется",
        },
        Def {
            key: "llm.visionDetected",
            kind: ConfigKind::Llm,
            label: "Кэш проверки vision",
            description: "Машинный кэш автоматической проверки поддержки изображений.",
            vtype: "bool|null",
            allowed: Value::Null,
            default: Value::Null,
            writable: false,
            deny: "машинный кэш автоопределения vision, менять не нужно",
        },
        Def {
            key: "app.autostart",
            kind: ConfigKind::App,
            label: "Запуск с системой",
            description: "Запускается ли приложение вместе с macOS.",
            vtype: "bool",
            allowed: Value::Null,
            default: json!(false),
            writable: false,
            deny: "автозапуск управляется через UI (LaunchAgent macOS)",
        },
        Def {
            key: "app.autopilot",
            kind: ConfigKind::App,
            label: "Автопилот",
            description: "Автоматическое подтверждение всех действий агента.",
            vtype: "bool",
            allowed: Value::Null,
            default: json!(false),
            writable: false,
            deny: "автопилот включается и выключается только в UI настроек",
        },
        Def {
            key: "app.sessionTtlDays",
            kind: ConfigKind::App,
            label: "Срок жизни сессий",
            description: "Через сколько дней удалять старые сессии Harness; 0 — не удалять.",
            vtype: "uint (дни)",
            allowed: Value::Null,
            default: json!(30),
            writable: false,
            deny: "ключ удаляет данные — меняется только в UI настроек",
        },
        Def {
            key: "app.tmpRetentionDays",
            kind: ConfigKind::App,
            label: "Срок хранения tmp",
            description: "Через сколько дней чистить Harness/tmp; 0 — не чистить.",
            vtype: "uint (дни)",
            allowed: Value::Null,
            default: json!(7),
            writable: false,
            deny: "ключ удаляет данные — меняется только в UI настроек",
        },
        Def {
            key: "app.logRetentionDays",
            kind: ConfigKind::App,
            label: "Срок хранения журнала",
            description: "За какой период хранится журнал действий; 0 — не удалять.",
            vtype: "uint (дни)",
            allowed: Value::Null,
            default: json!(14),
            writable: false,
            deny: "ключ удаляет данные — меняется только в UI настроек",
        },
        Def {
            key: "indexing.semanticSearch",
            kind: ConfigKind::Indexing,
            label: "Семантический поиск",
            description: "Семантический поиск всегда включён, отдельного ключа включения нет.",
            vtype: "read-only",
            allowed: Value::Null,
            default: json!("всегда включён"),
            writable: false,
            deny: "семантический поиск всегда включён, ключа включения нет",
        },
    ]
}

// Known keys outside the registry, with the reason they stay UI-only.
const FORBIDDEN: &[(&str, &str)] = &[
    ("llm.baseUrl", "адрес провайдера меняется только в UI настроек"),
    ("llm.providers", "провайдеры и их доступы меняются только в UI настроек"),
    ("llm.activeProvider", "выбор активного провайдера меняется только в UI настроек"),
    ("indexing.enabled", "такого ключа нет: семантический поиск всегда включён"),
];

fn forbidden_reason(key: &str) -> Option<String> {
    if let Some((_, reason)) = FORBIDDEN.iter().find(|(k, _)| *k == key) {
        return Some(reason.to_string());
    }
    if key.starts_with("mcp.") || key.starts_with("ssh.") {
        return Some("MCP-серверы и SSH-профили настраиваются только через UI настроек".to_string());
    }
    None
}

fn field_of(key: &str) -> String {
    key.split_once('.').map(|(_, f)| f.to_string()).unwrap_or_else(|| key.to_string())
}

fn load_configs(wd: &std::path::Path) -> Result<(Value, Value, Value)> {
    Ok((
        config::read_config(wd, ConfigKind::Llm)?,
        config::read_config(wd, ConfigKind::App)?,
        config::read_config(wd, ConfigKind::Indexing)?,
    ))
}

fn active_provider(llm: &Value) -> Option<&Value> {
    let id = llm.get("activeProvider").and_then(Value::as_str)?;
    llm.get("providers")
        .and_then(Value::as_array)?
        .iter()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(id))
}

fn provider_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split('/').next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    if host.is_empty() {
        "—".to_string()
    } else {
        host.to_string()
    }
}

fn current_value(def: &Def, llm: &Value, app: &Value, indexing: &Value) -> Value {
    match def.key {
        "llm.apiKey" => {
            let key = active_provider(llm)
                .or(Some(llm))
                .and_then(|p| p.get("apiKey").and_then(Value::as_str))
                .unwrap_or("");
            json!(if key.trim().is_empty() { "empty" } else { "set" })
        }
        "llm.providerName" => json!(active_provider(llm)
            .and_then(|p| p.get("name").and_then(Value::as_str))
            .filter(|n| !n.trim().is_empty())
            .unwrap_or("—")),
        "llm.providerHost" => {
            let url = active_provider(llm)
                .and_then(|p| p.get("baseUrl").and_then(Value::as_str))
                .or_else(|| llm.get("baseUrl").and_then(Value::as_str))
                .unwrap_or("");
            json!(provider_host(url))
        }
        _ => {
            let cfg = match def.kind {
                ConfigKind::Llm => llm,
                ConfigKind::App => app,
                ConfigKind::Indexing => indexing,
                _ => return Value::Null,
            };
            cfg.get(field_of(def.key))
                .cloned()
                .unwrap_or_else(|| def.default.clone())
        }
    }
}

fn entry_json(def: &Def, llm: &Value, app: &Value, indexing: &Value) -> Value {
    json!({
        "key": def.key,
        "category": def.kind.which(),
        "label": def.label,
        "description": def.description,
        "type": def.vtype,
        "allowed": def.allowed,
        "current": current_value(def, llm, app, indexing),
        "default": def.default,
        "writable": def.writable,
    })
}

fn uint_value(value: &Value, min: u64, max: u64, msg: &str) -> std::result::Result<Value, String> {
    let n = match value {
        Value::Number(_) => value.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
    .ok_or_else(|| msg.to_string())?;
    if n < min || n > max {
        return Err(format!("{msg}; допустимо {min}..{max}"));
    }
    Ok(json!(n))
}

// Validates and normalizes a proposed value per key (units as stored in the
// config: bytes for maxFileSizeBytes, days for the app counters).
fn normalize(key: &str, value: &Value) -> std::result::Result<Value, String> {
    match key {
        "llm.model" => {
            let text = value.as_str().map(str::trim).filter(|s| !s.is_empty());
            text.map(|s| json!(s))
                .ok_or_else(|| "«model» — непустая строка с названием модели".to_string())
        }
        "llm.thinkingLevel" => {
            let level = value
                .as_str()
                .map(|s| s.trim().to_ascii_lowercase())
                .unwrap_or_default();
            if matches!(level.as_str(), "off" | "low" | "medium" | "high") {
                Ok(json!(level))
            } else {
                Err("«thinkingLevel» допускает только off, low, medium или high".to_string())
            }
        }
        "llm.contextWindow" => uint_value(value, 1, u64::MAX, "«contextWindow» — целое число токенов ≥ 1"),
        "llm.maxTokens" => {
            if value.is_null() {
                Ok(Value::Null)
            } else {
                uint_value(value, 1, u32::MAX as u64, "«maxTokens» — целое ≥ 1 или null (без лимита)")
            }
        }
        "llm.vision" => match value {
            Value::Null => Ok(Value::Null),
            Value::Bool(b) => Ok(json!(b)),
            Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
                "auto" => Ok(Value::Null),
                "yes" | "true" | "да" => Ok(json!(true)),
                "no" | "false" | "нет" => Ok(json!(false)),
                _ => Err("«vision» допускает auto, yes или no".to_string()),
            },
            _ => Err("«vision» допускает auto, yes или no".to_string()),
        },
        "indexing.maxFileSizeBytes" => {
            uint_value(value, 1, u64::MAX, "«maxFileSizeBytes» — размер в байтах, целое > 0")
        }
        "indexing.maxTextChars" => {
            uint_value(value, 1, u64::MAX, "«maxTextChars» — число символов, целое > 0")
        }
        "indexing.excludeDirs" => {
            let items: std::result::Result<Vec<String>, String> = match value {
                Value::Array(list) => list
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .ok_or_else(|| {
                                "«excludeDirs» — массив непустых строк или строка через запятую".to_string()
                            })
                    })
                    .collect(),
                Value::String(text) => Ok(text
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()),
                _ => Err("«excludeDirs» — массив непустых строк или строка через запятую".to_string()),
            };
            Ok(json!(items?))
        }
        "app.backupIntervalDays" => {
            uint_value(value, 0, u64::MAX, "«backupIntervalDays» — целое ≥ 0 (0 = выключено)")
        }
        "app.backupKeep" => uint_value(value, 0, u64::MAX, "«backupKeep» — целое ≥ 0"),
        "app.compactionThresholdPercent" => uint_value(
            value,
            10,
            95,
            "«compactionThresholdPercent» — процент от 10 до 95",
        ),
        other => Err(format!("неизвестный ключ «{other}»")),
    }
}

struct Pending {
    key: String,
    kind: ConfigKind,
    field: String,
    value: Value,
    expires: Instant,
}

static PROPOSALS: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();

fn proposals() -> &'static Mutex<HashMap<String, Pending>> {
    PROPOSALS.get_or_init(Default::default)
}

fn new_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("st{nanos}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
fn expire_proposal(token: &str) {
    if let Some(p) = proposals().lock().unwrap().get_mut(token) {
        p.expires = Instant::now() - Duration::from_secs(1);
    }
}

fn invalid(message: impl Into<String>) -> ToolResult {
    ToolResult::err(ToolErrorCode::InvalidArgs, message)
}

pub struct SettingsListTool;

impl Tool for SettingsListTool {
    fn name(&self) -> &str {
        "settings_list"
    }
    fn description(&self) -> &str {
        "Показывает реальные настройки Project-S из Settings/config/*.json: ключ, метку, влияние, тип, допустимые значения, ТЕКУЩЕЕ значение, default и writable; category: llm|app|indexing.\nСекреты показаны только как set/empty. Перед любым изменением сначала вызови settings_list, чтобы видеть актуальные ключ и значение, а не догадываться."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "category": { "type": "string", "enum": ["llm", "app", "indexing"] }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = ctx.working_dir.clone().ok_or(AppError::NotConfigured)?;
        let category = args.get("category").and_then(Value::as_str);
        let (llm, app, indexing) = load_configs(&wd)?;
        let entries: Vec<Value> = defs()
            .iter()
            .filter(|d| category.map_or(true, |c| d.kind.which() == c))
            .map(|d| entry_json(d, &llm, &app, &indexing))
            .collect();
        Ok(ToolResult::ok(json!({ "settings": entries, "count": entries.len() }))
            .capped(self.max_result_len()))
    }
}

pub struct SettingsProposeTool;

impl Tool for SettingsProposeTool {
    fn name(&self) -> &str {
        "settings_propose"
    }
    fn description(&self) -> &str {
        "Готовит изменение ОДНОЙ настройки (ключ из settings_list), проверяет значение и возвращает текущее/предлагаемое/допустимые значения плюс одноразовый токен settings_apply (5 минут); на диск НЕ пишет.\nПорядок обязателен: 1) settings_list; 2) settings_propose; 3) объяснить пользователю простыми русскими словами, что это за настройка и на что повлияет, показать «сейчас → станет» и допустимые значения; 4) спросить разрешение и дождаться ЯВНОГО согласия в чате; 5) только после согласия вызвать settings_apply(token).\nБез явного «да» применять нельзя. Запрещённые ключи (секреты, провайдеры, пути, MCP/SSH, удаление данных) отклоняются с объяснением."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string" },
                "value": {}
            },
            "required": ["key", "value"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Meta
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = ctx.working_dir.clone().ok_or(AppError::NotConfigured)?;
        let key = args["key"].as_str().unwrap_or("").trim().to_string();
        let def = defs().into_iter().find(|d| d.key == key);
        let Some(def) = def else {
            let reason = forbidden_reason(&key).unwrap_or_else(|| {
                "такого ключа нет в реальных конфигах; полный список — в settings_list".to_string()
            });
            return Ok(invalid(format!(
                "Ключ «{key}» нельзя менять из чата: {reason}."
            )));
        };
        if !def.writable {
            return Ok(invalid(format!(
                "Ключ «{key}» нельзя менять из чата: {}.",
                def.deny
            )));
        }
        let normalized = match normalize(&key, &args["value"]) {
            Ok(v) => v,
            Err(msg) => return Ok(invalid(msg)),
        };
        let (llm, app, indexing) = load_configs(&wd)?;
        let current = current_value(&def, &llm, &app, &indexing);
        let token = new_token();
        proposals().lock().unwrap().insert(
            token.clone(),
            Pending {
                key: key.clone(),
                kind: def.kind,
                field: field_of(&key),
                value: normalized.clone(),
                expires: Instant::now() + PROPOSAL_TTL,
            },
        );
        Ok(ToolResult::ok(json!({
            "key": key,
            "label": def.label,
            "description": def.description,
            "current": current,
            "proposed": normalized,
            "allowed": def.allowed,
            "default": def.default,
            "token": token,
            "expiresInSeconds": PROPOSAL_TTL.as_secs(),
            "next": "Покажи пользователю «сейчас → станет», объясни влияние простыми русскими словами и спроси разрешение. settings_apply вызывай только после явного согласия пользователя.",
        })))
    }
}

pub struct SettingsApplyTool;

impl Tool for SettingsApplyTool {
    fn name(&self) -> &str {
        "settings_apply"
    }
    fn description(&self) -> &str {
        "Применяет изменение, подготовленное settings_propose, по одноразовому токену (валиден 5 минут); токен сгорает после первого применения.\nВызывай ТОЛЬКО после того, как пользователь явно подтвердил в чате конкретное изменение «текущее → новое»; никогда не применяй без явного «да» пользователя. Пишет в тот же файл настроек, что и UI; интерфейс обновится сам."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "token": { "type": "string" } },
            "required": ["token"],
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
        let wd = ctx.working_dir.clone().ok_or(AppError::NotConfigured)?;
        let token = args["token"].as_str().unwrap_or("").trim();
        let pending = proposals().lock().unwrap().remove(token);
        let Some(pending) = pending else {
            return Ok(invalid(
                "Токен не найден или уже использован. Сначала вызови settings_propose заново.",
            ));
        };
        if pending.expires < Instant::now() {
            return Ok(invalid(
                "Предложение устарело (5 минут). Вызови settings_propose заново.",
            ));
        }
        if let Err(e) = config::write_config(&wd, pending.kind, json!({ (pending.field): pending.value })) {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!("не удалось записать настройку: {e}"),
            ));
        }
        Ok(ToolResult::ok(json!({
            "applied": { "key": pending.key, "value": pending.value },
            "note": "Значение записано в тот же файл, который правит UI настроек; интерфейс обновится сам.",
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness_tools::test_support::{exec, setup};
    use crate::agent::project_tools::project_catalog;
    use crate::config::file_path;
    use serde_json::json;
    use std::fs;

    fn entry<'a>(entries: &'a [Value], key: &str) -> &'a Value {
        entries
            .iter()
            .find(|e| e["key"] == key)
            .unwrap_or_else(|| panic!("registry entry {key} missing"))
    }

    fn write_raw(wd: &std::path::Path, kind: ConfigKind, text: &str) {
        fs::create_dir_all(crate::paths::config_dir(wd)).unwrap();
        fs::write(file_path(wd, kind), text).unwrap();
    }

    #[test]
    fn registry_shows_real_current_values_and_hides_secrets() {
        let env = setup("settings-registry");
        write_raw(
            &env.wd,
            ConfigKind::Llm,
            r#"{ "model": "gpt-4o", "apiKey": "sk-SECRET-9", "baseUrl": "https://api.example.com/v1",
                 "contextWindow": 128000, "temperature": 0.7,
                 "providers": [{"id":"p1","name":"Example","baseUrl":"https://api.example.com/v1","apiKey":"sk-SECRET-9"}],
                 "activeProvider": "p1", "customFlag": true }"#,
        );

        let r = exec("settings_list", json!({}), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let entries = data["settings"].as_array().unwrap();

        assert_eq!(entry(entries, "llm.model")["current"], "gpt-4o");
        assert_eq!(entry(entries, "llm.model")["writable"], true);
        assert_eq!(entry(entries, "llm.apiKey")["current"], "set");
        assert_eq!(entry(entries, "llm.apiKey")["writable"], false);
        assert_eq!(entry(entries, "llm.providerName")["current"], "Example");
        assert_eq!(entry(entries, "llm.providerHost")["current"], "api.example.com");
        assert_eq!(entry(entries, "llm.temperature")["current"], 0.7);
        // missing files fall back to the shipped defaults
        assert_eq!(entry(entries, "app.compactionThresholdPercent")["current"], 80);
        assert_eq!(entry(entries, "indexing.maxFileSizeBytes")["current"], 10 * 1024 * 1024);
        assert_eq!(entry(entries, "indexing.semanticSearch")["writable"], false);

        // the secret value never reaches the model, only the safe host does
        let rendered = serde_json::to_string(&data).unwrap();
        assert!(!rendered.contains("sk-SECRET-9"), "secret leaked");
        assert!(!rendered.contains("https://api.example.com"), "full url leaked");

        // category filter keeps only that config kind
        let r = exec("settings_list", json!({ "category": "indexing" }), &env.ctx);
        let entries = r.data.unwrap()["settings"].as_array().unwrap().clone();
        assert!(!entries.is_empty());
        assert!(entries.iter().all(|e| e["category"] == "indexing"));
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn propose_apply_round_trip_writes_config_and_preserves_unknown_keys() {
        let env = setup("settings-roundtrip");
        write_raw(
            &env.wd,
            ConfigKind::Llm,
            r#"{ "model": "old-model", "customFlag": true }"#,
        );

        let r = exec(
            "settings_propose",
            json!({ "key": "llm.model", "value": "gpt-5" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["current"], "old-model");
        assert_eq!(data["proposed"], "gpt-5");
        assert!(data["token"].as_str().unwrap().starts_with("st"));
        let token = data["token"].as_str().unwrap().to_string();

        let r = exec("settings_apply", json!({ "token": token }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["applied"]["value"], "gpt-5");
        let llm = config::read_config(&env.wd, ConfigKind::Llm).unwrap();
        assert_eq!(llm["model"], "gpt-5");
        assert_eq!(llm["customFlag"], true, "unknown keys must survive");

        // comma-separated excludeDirs normalizes to the array the config stores
        let r = exec(
            "settings_propose",
            json!({ "key": "indexing.excludeDirs", "value": "node_modules, .git, vendor" }),
            &env.ctx,
        );
        let token = r.data.unwrap()["token"].as_str().unwrap().to_string();
        assert!(exec("settings_apply", json!({ "token": token }), &env.ctx).ok);
        let indexing = config::read_config(&env.wd, ConfigKind::Indexing).unwrap();
        assert_eq!(indexing["excludeDirs"], json!(["node_modules", ".git", "vendor"]));

        // null clears maxTokens
        let r = exec(
            "settings_propose",
            json!({ "key": "llm.maxTokens", "value": null }),
            &env.ctx,
        );
        let token = r.data.unwrap()["token"].as_str().unwrap().to_string();
        assert!(exec("settings_apply", json!({ "token": token }), &env.ctx).ok);
        assert!(config::read_config(&env.wd, ConfigKind::Llm).unwrap()["maxTokens"].is_null());

        // app percent setting lands in app.json with the stored unit
        let r = exec(
            "settings_propose",
            json!({ "key": "app.compactionThresholdPercent", "value": 70 }),
            &env.ctx,
        );
        let token = r.data.unwrap()["token"].as_str().unwrap().to_string();
        assert!(exec("settings_apply", json!({ "token": token }), &env.ctx).ok);
        assert_eq!(
            config::read_config(&env.wd, ConfigKind::App).unwrap()["compactionThresholdPercent"],
            70
        );
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn proposal_token_is_single_use_and_expires() {
        let env = setup("settings-token");
        let r = exec(
            "settings_propose",
            json!({ "key": "llm.thinkingLevel", "value": "high" }),
            &env.ctx,
        );
        let token = r.data.unwrap()["token"].as_str().unwrap().to_string();
        assert!(exec("settings_apply", json!({ "token": token.clone() }), &env.ctx).ok);

        let again = exec("settings_apply", json!({ "token": token }), &env.ctx);
        assert_eq!(again.error.as_ref().unwrap().code, "invalid_args");
        assert!(again.error.as_ref().unwrap().message.contains("использован"));

        let r = exec(
            "settings_propose",
            json!({ "key": "llm.thinkingLevel", "value": "low" }),
            &env.ctx,
        );
        let token = r.data.unwrap()["token"].as_str().unwrap().to_string();
        expire_proposal(&token);
        let expired = exec("settings_apply", json!({ "token": token }), &env.ctx);
        assert_eq!(expired.error.as_ref().unwrap().code, "invalid_args");
        assert!(expired.error.as_ref().unwrap().message.contains("устарело"));
        assert_ne!(
            config::read_config(&env.wd, ConfigKind::Llm).unwrap()["thinkingLevel"],
            "low"
        );

        let bogus = exec("settings_apply", json!({ "token": "nope" }), &env.ctx);
        assert_eq!(bogus.error.as_ref().unwrap().code, "invalid_args");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn forbidden_and_readonly_keys_are_rejected_without_writing() {
        let env = setup("settings-forbidden");
        write_raw(
            &env.wd,
            ConfigKind::Llm,
            r#"{ "model": "keep-me", "apiKey": "sk-keep", "baseUrl": "https://keep.example/v1" }"#,
        );
        write_raw(
            &env.wd,
            ConfigKind::App,
            r#"{ "autostart": false, "sessionTtlDays": 30, "autopilot": false }"#,
        );

        for key in [
            "llm.apiKey",
            "llm.baseUrl",
            "llm.providers",
            "llm.activeProvider",
            "llm.temperature",
            "llm.visionDetected",
            "llm.providerName",
            "app.autostart",
            "app.autopilot",
            "app.sessionTtlDays",
            "app.tmpRetentionDays",
            "app.logRetentionDays",
            "indexing.enabled",
            "indexing.semanticSearch",
            "mcp.servers",
            "ssh.profiles",
            "totally.unknown",
        ] {
            let r = exec("settings_propose", json!({ "key": key, "value": "x" }), &env.ctx);
            assert_eq!(r.error.as_ref().unwrap().code, "invalid_args", "{key} must be rejected");
            assert!(
                r.error.as_ref().unwrap().message.contains("нельзя менять из чата"),
                "{key}: {:#?}",
                r.error
            );
        }
        assert_eq!(config::read_config(&env.wd, ConfigKind::Llm).unwrap()["model"], "keep-me");
        assert_eq!(config::read_config(&env.wd, ConfigKind::App).unwrap()["sessionTtlDays"], 30);
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn invalid_enums_and_numbers_are_rejected() {
        let env = setup("settings-invalid");
        for (key, value) in [
            ("llm.model", json!("   ")),
            ("llm.thinkingLevel", json!("ultra")),
            ("llm.contextWindow", json!(0)),
            ("llm.contextWindow", json!(-5)),
            ("llm.maxTokens", json!(0)),
            ("llm.vision", json!("maybe")),
            ("indexing.maxFileSizeBytes", json!(0)),
            ("indexing.maxTextChars", json!("abc")),
            ("indexing.excludeDirs", json!([1, 2])),
            ("app.compactionThresholdPercent", json!(96)),
            ("app.compactionThresholdPercent", json!(9)),
            ("app.backupKeep", json!(-1)),
        ] {
            let r = exec("settings_propose", json!({ "key": key, "value": value }), &env.ctx);
            assert_eq!(
                r.error.as_ref().unwrap().code,
                "invalid_args",
                "{key}={value} must be rejected"
            );
        }
        // numeric strings coerce like the shared arg coercion would
        let r = exec(
            "settings_propose",
            json!({ "key": "app.backupIntervalDays", "value": "3" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["proposed"], 3);
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn tools_are_registered_in_both_catalogs_with_chat_confirmation_flow() {
        for name in ["settings_list", "settings_propose", "settings_apply"] {
            let project = project_catalog();
            assert!(
                matches!(project.resolve(name), crate::agent::catalog::Resolution::Found(_)),
                "{name} missing from project catalog"
            );
            let tool = crate::agent::harness_tools::test_support::tool(name);
            assert_eq!(tool.policy(), PermissionPolicy::Skip, "{name} policy");
            let schema = tool.parameters();
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
        }
        assert_eq!(SettingsListTool.class(), ToolClass::ReadOnly);
        assert_eq!(SettingsProposeTool.class(), ToolClass::Meta);
        assert_eq!(SettingsApplyTool.class(), ToolClass::Mutating);
        // the descriptions carry the chat-confirmation contract
        let propose = SettingsProposeTool.description();
        assert!(propose.contains("settings_apply"));
        assert!(propose.contains("ЯВНОГО"));
        let apply = SettingsApplyTool.description();
        assert!(apply.contains("ТОЛЬКО после"));
        assert!(apply.contains("явного «да»"));
    }
}

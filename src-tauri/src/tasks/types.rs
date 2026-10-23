use serde::{Deserialize, Serialize};

pub const STATUSES: [&str; 3] = ["new", "progress", "done"];
pub const MAX_CHECKLIST_ITEMS: usize = 100;
pub const DEFAULT_DUE_TIME: &str = "07:00";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistItem {
    pub text: String,
    #[serde(default)]
    pub done: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDto {
    pub id: i64,
    pub project_id: i64,
    pub title: String,
    pub status: String,
    pub comment: Option<String>,
    pub due_date: Option<String>,
    pub due_time: String,
    pub remind_at: Option<String>,
    pub position: f64,
    pub created_at: String,
    pub closed_at: Option<String>,
    pub overdue: bool,
    pub is_priority: bool,
    pub checklist: Vec<ChecklistItem>,
    pub recurrence: String,
    // Wall-clock timer (SPEC 4.3): accumulated spent ms plus the active
    // start instant in epoch ms (UTC); started_at is set exactly while the
    // task sits in 'progress'. The UI derives the live total as
    // spentMs + (now - startedAt) while running.
    pub spent_ms: i64,
    pub started_at: Option<i64>,
}

// Distinguishes an explicit null (Some(None) -> clear) from an absent field (None -> keep).
mod opt_opt_str {
    use serde::de::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer).map(Some)
    }
}

// Absent field leaves the value unchanged; explicit null clears it.
// is_priority/checklist are NOT NULL columns: an explicit null is
// indistinguishable from absent and keeps the current value. due_time is
// NOT NULL too: null/absent keeps it, a set value must be HH:MM.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskPatch {
    pub title: Option<String>,
    #[serde(deserialize_with = "opt_opt_str::deserialize")]
    pub comment: Option<Option<String>>,
    #[serde(deserialize_with = "opt_opt_str::deserialize")]
    pub due_date: Option<Option<String>>,
    pub due_time: Option<String>,
    #[serde(deserialize_with = "opt_opt_str::deserialize")]
    pub remind_at: Option<Option<String>>,
    pub is_priority: Option<bool>,
    pub checklist: Option<Vec<ChecklistItem>>,
}

// Full tasks row as JSON, snapshotted into the trash table before delete
// (kind='task', no file on disk); re-inserted by task_restore_from_trash.
// The serde defaults keep snapshots written before v25 restorable.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskSnapshot {
    pub(crate) id: i64,
    pub(crate) project_id: i64,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) comment: Option<String>,
    pub(crate) due_date: Option<String>,
    #[serde(default = "default_due_time")]
    pub(crate) due_time: String,
    pub(crate) remind_at: Option<String>,
    pub(crate) tags: String,
    pub(crate) position: f64,
    pub(crate) created_at: String,
    pub(crate) closed_at: Option<String>,
    #[serde(default = "default_priority")]
    pub(crate) priority: String,
    #[serde(default = "default_empty_json")]
    pub(crate) checklist: String,
    #[serde(default = "default_recurrence")]
    pub(crate) recurrence: String,
    #[serde(default)]
    pub(crate) is_priority: bool,
    // Timer state rides along so undo/trash restore puts the task back with
    // exactly the spent time and active-start stamp it had at snapshot time.
    // The serde defaults keep snapshots written before v33 restorable.
    #[serde(default)]
    pub(crate) spent_ms: i64,
    #[serde(default)]
    pub(crate) started_at: Option<i64>,
}

fn default_priority() -> String {
    "normal".to_owned()
}

fn default_empty_json() -> String {
    "[]".to_owned()
}

fn default_recurrence() -> String {
    "none".to_owned()
}

fn default_due_time() -> String {
    DEFAULT_DUE_TIME.to_owned()
}

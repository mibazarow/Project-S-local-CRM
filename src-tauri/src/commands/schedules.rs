use crate::{scheduler};
use crate::error::{Result};
use crate::state::{AppState};
use super::{with_db, emit_schedules_changed};
use tauri::{AppHandle, Emitter, State};

// Scheduler (SPEC 5.3): run_at / cron+IANA tz / notify over the schedules
// table; the background thread ticks every 30 s and catches up missed runs
// on the first tick after start. The UI list screen is a later slice.
#[tauri::command]
pub(crate) fn schedule_list(state: State<AppState>) -> Result<Vec<scheduler::ScheduleDto>> {
    with_db(&state, scheduler::list)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) fn schedule_create(
    app: AppHandle,
    state: State<AppState>,
    kind: String,
    spec: String,
    prompt: Option<String>,
    timezone: Option<String>,
    session_id: Option<i64>,
    title: Option<String>,
) -> Result<scheduler::ScheduleDto> {
    let dto = with_db(&state, |conn| {
        scheduler::create(
            conn,
            &scheduler::CreateIn {
                kind: &kind,
                spec: &spec,
                prompt: prompt.as_deref(),
                timezone: timezone.as_deref(),
                session_id,
                related_task_id: None,
                title: title.as_deref(),
            },
            scheduler::now_utc(),
        )
    })?;
    emit_schedules_changed(&app);
    Ok(dto)
}

#[tauri::command]
pub(crate) fn schedule_pause(app: AppHandle, state: State<AppState>, id: i64) -> Result<scheduler::ScheduleDto> {
    let dto = with_db(&state, |conn| scheduler::pause(conn, id))?;
    emit_schedules_changed(&app);
    Ok(dto)
}

#[tauri::command]
pub(crate) fn schedule_resume(app: AppHandle, state: State<AppState>, id: i64) -> Result<scheduler::ScheduleDto> {
    let dto = with_db(&state, |conn| scheduler::resume(conn, id, scheduler::now_utc()))?;
    emit_schedules_changed(&app);
    Ok(dto)
}

#[tauri::command]
pub(crate) fn schedule_delete(app: AppHandle, state: State<AppState>, id: i64) -> Result<()> {
    with_db(&state, |conn| scheduler::delete(conn, id))?;
    emit_schedules_changed(&app);
    Ok(())
}

// Click-through from the schedules panel: returns the session whose chat the
// UI opens (the schedule's own, or a fresh one titled after the schedule);
// "sessions-changed" makes a freshly created session appear in the sidebar.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScheduleOpenResult {
    session_id: i64,
}

#[tauri::command]
pub(crate) fn schedule_open(app: AppHandle, state: State<AppState>, id: i64) -> Result<ScheduleOpenResult> {
    let session_id = with_db(&state, |conn| scheduler::open_session(conn, id))?;
    let _ = app.emit("sessions-changed", serde_json::json!({}));
    Ok(ScheduleOpenResult { session_id })
}

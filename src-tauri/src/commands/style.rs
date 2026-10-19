use crate::error::Result;
use crate::state::AppState;
use crate::style;
use super::with_db;
use tauri::State;

// Reset the global style-of-communication profile (SPEC 6.6): the next runs
// start from a neutral profile until the user's own messages rebuild it.
#[tauri::command]
pub(crate) fn style_reset(state: State<AppState>) -> Result<()> {
    with_db(&state, style::reset)
}

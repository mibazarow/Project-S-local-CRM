// Native notifications (SPEC 5.3): they go through tauri-plugin-notification;
// on macOS the first notification triggers the OS permission prompt. The
// trait lets tests inject a recording notifier.
use crate::error::{AppError, Result};
use tauri::AppHandle;

// Q.md item 7: the notification action type that makes a completion
// notification clickable; the click handler in lib.rs reports the body
// click as "tap" and the button as "open".
pub const CLICK_ACTION_TYPE: &str = "schedule";

pub trait Notifier {
    fn notify(&self, title: &str, body: &str) -> Result<()>;

    // Q.md item 7: a scheduled run's completion notification carries the
    // schedule and session ids as extras and references the registered
    // click action type, so clicking it opens the schedule's chat. The
    // default drops the ids (plain delivery); tests keep the plain path.
    fn notify_completion(
        &self,
        title: &str,
        body: &str,
        schedule_id: i64,
        session_id: i64,
    ) -> Result<()> {
        let _ = (schedule_id, session_id);
        self.notify(title, body)
    }
}

pub struct TauriNotifier<'a> {
    app: &'a AppHandle,
}

impl<'a> TauriNotifier<'a> {
    pub fn new(app: &'a AppHandle) -> Self {
        TauriNotifier { app }
    }
}

impl Notifier for TauriNotifier<'_> {
    fn notify(&self, title: &str, body: &str) -> Result<()> {
        use tauri_plugin_notification::NotificationExt;
        self.app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
            .map_err(|e| AppError::Io(format!("notification: {e}")))
    }

    fn notify_completion(
        &self,
        title: &str,
        body: &str,
        schedule_id: i64,
        session_id: i64,
    ) -> Result<()> {
        use tauri_plugin_notification::NotificationExt;
        self.app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .action_type_id(CLICK_ACTION_TYPE)
            .extra("scheduleId", schedule_id)
            .extra("sessionId", session_id)
            .show()
            .map_err(|e| AppError::Io(format!("notification: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::testutil::RecordingNotifier;

    #[test]
    fn completion_notifications_fall_back_to_plain_delivery_in_recording_testers() {
        // The trait default drops the schedule/session ids and delivers a
        // plain notification, so every existing recording tester keeps the
        // completion path observable.
        let notifier = RecordingNotifier::new();
        notifier.notify_completion("t", "b", 1, 2).unwrap();
        assert_eq!(
            *notifier.sent.lock().unwrap(),
            vec![("t".to_string(), "b".to_string())]
        );
    }
}

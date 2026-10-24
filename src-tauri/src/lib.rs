mod agent;
mod attachments;
mod brief;
mod cdp;
mod chat;
mod commands;
mod config;
mod confirm;
mod db;
mod embed;
mod error;
mod extract;
mod export;
mod fs_ops;
mod harness;
mod index;
mod importer;
mod journal;
mod license;
mod links;
mod llm;
mod mcp;
mod memory;
mod notes;
mod objectives;
mod onboarding;
mod open_with;
mod ops;
mod path_policy;
mod platform;
mod paths;
mod processes;
mod projects;
mod reliability;
mod scheduler;
mod search;
mod search_reindex;
mod skills;
mod state;
mod style;
pub mod sync_util;
mod tasks;
mod templates;
mod todos;
mod undo;
mod watcher;

use commands::{app_config_dir, open_journal, session_ttl_days, spawn_auto_backup};
use state::{AgentRegistry, AppState};
use sync_util::lock_recover;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};

// Menu-bar (tray) entry point (SPEC quick capture slice): a left click on the
// tray icon shows/focuses the main window; the menu routes «Быстрая заметка»
// and «Новая задача» to the frontend as "tray-action" events (the window is
// shown first), «Выход» exits natively.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TrayAction {
    action: String,
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let open = MenuItem::with_id(app, "open", "Открыть Project-S", true, None::<&str>)?;
    let note = MenuItem::with_id(app, "quick-note", "Быстрая заметка", true, None::<&str>)?;
    let task = MenuItem::with_id(app, "new-task", "Новая задача", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &note,
            &task,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            match event.id.as_ref() {
                "quit" => app.exit(0),
                action @ ("open" | "quick-note" | "new-task") => {
                    show_main_window(app);
                    let _ = app.emit(
                        "tray-action",
                        TrayAction {
                            action: action.into(),
                        },
                    );
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

// Global CmdOrCtrl+Shift+Space (SUPER on macOS, CTRL elsewhere): shows the
// window and asks the frontend to open the capture modal. A registration
// failure (the shortcut is already taken) is logged, never fatal.
fn setup_global_shortcut(app: &AppHandle) {
    use tauri_plugin_global_shortcut::{
        Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
    };

    for modifiers in [
        Modifiers::SUPER | Modifiers::SHIFT,
        Modifiers::CONTROL | Modifiers::SHIFT,
    ] {
        let shortcut = Shortcut::new(Some(modifiers), Code::Space);
        let result = app.global_shortcut().on_shortcut(shortcut, |app, _gs, event| {
            if event.state == ShortcutState::Pressed {
                show_main_window(app);
                let _ = app.emit("quick-capture", serde_json::json!({}));
            }
        });
        if let Err(e) = result {
            eprintln!("global shortcut {shortcut:?} registration failed: {e}");
        }
    }
}

// Q.md item 7: schedule-run completion notifications must be clickable.
// Registering the action type makes the notification carry its buttons, and
// the on_action handler is what makes the notification server report clicks
// at all: a body click arrives as "tap", the button as "open". Both read the
// extras written by scheduler::TauriNotifier::notify_completion, show the
// main window and route the frontend to the schedule's chat through the
// "schedule-open" event. Registered in setup before any notification is
// sent; a registration failure is never fatal.
fn setup_schedule_notifications(app: &AppHandle) {
    use tauri_plugin_notification::{Action, ActionType, NotificationExt};

    let notification = app.notification();
    let _ = notification.register_action_types(vec![ActionType::builder(scheduler::CLICK_ACTION_TYPE)
        .actions(vec![Action::builder("open", "Открыть чат").build()])
        .build()]);
    let app = app.clone();
    let _ = notification.on_action(move |action| {
        if !matches!(action.action_id(), "tap" | "open") {
            return;
        }
        let Some(session_id) = action
            .notification()
            .and_then(|n| n.extra().get("sessionId"))
            .and_then(|v| v.as_i64())
        else {
            return;
        };
        show_main_window(&app);
        let _ = app.emit("schedule-open", serde_json::json!({ "sessionId": session_id }));
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        // SPEC 5.3: "запускаться с системой" as a macOS LaunchAgent.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // Quick capture global shortcut (registered in setup below).
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(AppState::new())
        .manage(Arc::new(AgentRegistry::new()))
        .setup(|app| {
            let app_cfg = app_config_dir(app.handle())?;
            let state = app.state::<AppState>();
            // Background emitters (the rename_chat, todowrite tools) reach
            // the windows through the app state handle.
            *lock_recover(&state.emitter) = Some(Arc::new(app.handle().clone()));
            // Bundled model: Contents/Resources on macOS, the install dir
            // (resource_dir) on Windows. The exe-dir fallback covers installs
            // where resource_dir is unavailable.
            let resource_root = app.path().resource_dir().ok().or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
            });
            if let Some(resource_root) = resource_root {
                embed::model_store::set_bundled_model_dir(
                    resource_root
                        .join("resources/models")
                        .join(embed::model_store::MODEL_DIR_NAME),
                );
            }
            if let Some(wd) = onboarding::load_working_dir(&app_cfg) {
                *lock_recover(&state.db) = Some(db::open(&paths::db_path(&wd))?);
                *lock_recover(&state.working_dir) = Some(wd.clone());
                // Clickable schedule notifications: the action type and the
                // click handler must exist before the first tick can send a
                // completion notification.
                setup_schedule_notifications(app.handle());
                // One-time indexing-limits migration for installs created
                // before the 10 MB / 1M chars defaults. Best-effort.
                let _ = config::migrate_indexing_defaults(&wd);
                // SPEC 6.2: keep the bundled builtin skill set in sync with
                // this app build on every start (missing/changed copies are
                // refreshed, builtin copies the bundle no longer ships are
                // removed; user skills are never touched).
                // Best-effort: a sync failure must never block startup.
                let _ = skills::sync_builtin(&wd);
                // SPEC 5.1: drop harness sessions idle past the TTL on start.
                let ttl = session_ttl_days(&wd);
                let guard = lock_recover(&state.db);
                if let Some(conn) = guard.as_ref() {
                    let _ = projects::ensure_builtin_project(conn, &wd);
                    let _ = harness::purge_inactive_sessions(conn, ttl);
                    // Background process rows from a previous run: the
                    // processes themselves were reparented to launchd and
                    // keep running; reconcile only updates their status,
                    // it never kills them.
                    let _ = processes::reconcile(conn);
                    // Trash/version retention (SPEC trash/versions slice):
                    // drop entries older than 30 days, best-effort.
                    reliability::startup_purge(conn, &wd);
                }
                drop(guard);
                // Notes vault: create/seed Notes/ before the watcher attaches
                // to it, then index it so notes are searchable right away.
                // Best-effort: a vault failure must never block startup.
                let _ = notes::ensure(&wd);
                open_journal(&state, &wd)?;
                watcher::start(app.handle(), &state, &wd);
                let _ = index::index_notes(&wd);
                *lock_recover(&state.mcp) = mcp::McpRegistry::start(&wd);
                scheduler::start(app.handle().clone());
                search_reindex::start_timer(app.handle().clone());
                spawn_auto_backup(&wd);
            }
            // Quick capture entry points: menu-bar item + global shortcut.
            setup_tray(app.handle())?;
            setup_global_shortcut(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::settings::ping,
            commands::onboarding::onboarding_status,
            commands::onboarding::default_working_dir,
            commands::onboarding::choose_working_dir,
            commands::onboarding::get_working_dir,
            commands::license::license_status,
            commands::license::license_accept,
            commands::license::license_decline,
            commands::settings::config_dir_path,
            commands::settings::read_config,
            commands::settings::write_config,
            commands::projects::list_projects,
            commands::projects::create_project,
            commands::projects::rename_project,
            commands::projects::delete_project,
            commands::projects::project_path,
            commands::projects::touch_project_opened,
            commands::projects::project_brief,
            commands::projects::refresh_project_brief,
            commands::projects::move_project,
            commands::projects::set_task_columns,
            commands::projects::project_set_files_view,
            commands::projects::project_set_archived,
            commands::tasks::tasks_list,
            commands::tasks::task_create,
            commands::tasks::task_update,
            commands::tasks::task_move,
            commands::tasks::task_delete,
            commands::links::link_create,
            commands::tasks::todos_list,
            commands::objectives::objective_create,
            commands::objectives::objective_list,
            commands::objectives::objective_update,
            commands::objectives::objective_step_done,
            commands::objectives::objective_delete,
            commands::files::fs_list_dir,
            commands::files::fs_read_text,
            commands::files::fs_write_text,
            commands::files::fs_create_folder,
            commands::files::fs_create_file,
            commands::files::fs_rename,
            commands::files::fs_copy_as,
            commands::files::fs_absolute_path,
            commands::files::fs_reveal_in_finder,
            commands::files::fs_extract_archive,
            commands::files::fs_delete,
            commands::files::fs_move,
            commands::files::fs_import,
            commands::files::fs_open_external,
            commands::files::list_open_with_apps,
            commands::files::get_open_with,
            commands::files::remember_open_with,
            commands::files::open_with,
            commands::files::fs_read_binary_data_url,
            commands::notes::notes_ensure,
            commands::notes::notes_tree,
            commands::notes::notes_list,
            commands::notes::notes_read,
            commands::notes::notes_read_binary_data_url,
            commands::notes::notes_write,
            commands::notes::notes_create,
            commands::notes::notes_mkdir,
            commands::notes::notes_rename,
            commands::notes::notes_move,
            commands::notes::notes_copy,
            commands::notes::notes_import,
            commands::notes::notes_reveal,
            commands::notes::notes_delete,
            commands::undo::undo_last,
            commands::undo::undo_list,
            commands::capture::quick_capture,
            commands::templates::templates_list,
            commands::templates::template_create_project,
            commands::templates::template_create_task,
            commands::templates::template_document,
            commands::trash::trash_list,
            commands::trash::trash_restore,
            commands::trash::trash_purge,
            commands::trash::version_list,
            commands::trash::version_restore,
            commands::notes::ensure_notes_chat,
            commands::notes::notes_chat_history,
            commands::notes::send_notes_message,
            commands::notes::cancel_notes_run,
            commands::search::search_notes,
            commands::index::index_notes,
            commands::attachments::attach_files,
            commands::attachments::attachment_data_url,
            commands::attachments::attachment_open,
            commands::attachments::chat_attachment_data_url,
            commands::attachments::chat_attachment_open,
            commands::index::index_project,
            commands::index::reindex_all,
            commands::index::reindex_hard,
            commands::index::index_status,
            commands::index::reindex_start,
            commands::index::reindex_status,
            commands::index::embed_status,
            commands::index::download_embed_model,
            commands::search::semantic_search,
            commands::search::search_project,
            commands::search::search_chats,
            commands::search::global_search,
            commands::index::index_chat_messages,
            commands::llm::list_models,
            commands::llm::test_connection,
            commands::llm::detect_vision,
            commands::llm::get_vision_status,
            commands::llm::llm_providers_list,
            commands::llm::llm_provider_add,
            commands::llm::llm_provider_update,
            commands::llm::llm_provider_delete,
            commands::llm::llm_provider_activate,
            commands::chat::send_project_message,
            commands::chat::resolve_confirmation,
            commands::chat::project_chat_history,
            commands::chat::cancel_run,
            commands::harness::harness_create_session,
            commands::harness::harness_list_sessions,
            commands::harness::harness_rename_session,
            commands::harness::harness_delete_session,
            commands::harness::harness_pin_session,
            commands::harness::harness_session_history,
            commands::chat::chat_pin,
            commands::chat::chat_truncate_after,
            commands::chat::chat_branch,
            commands::chat::chat_search,
            commands::harness::send_session_message,
            commands::harness::cancel_session_run,
            commands::harness::purge_sessions,
            commands::schedules::schedule_list,
            commands::schedules::schedule_pause,
            commands::schedules::schedule_resume,
            commands::schedules::schedule_delete,
            commands::schedules::schedule_open,
            commands::memory::memory_list,
            commands::memory::memory_add,
            commands::memory::memory_update,
            commands::memory::memory_delete,
            commands::memory::memory_search,
            commands::memory::index_memory,
            commands::style::style_reset,
            commands::mcp::mcp_status,
            commands::mcp::mcp_reload,
            commands::mcp::mcp_set_enabled,
            commands::skills::skills_list,
            commands::skills::skills_set_enabled,
            commands::skills::skills_import,
            commands::skills::skills_install_builtin,
            commands::skills::skills_create,
            commands::ops::create_backup,
            commands::ops::clear_tmp,
            commands::ops::backups_dir,
            commands::ops::backup_status,
            commands::ops::import_folder,
            commands::ops::import_archive,
            commands::ops::import_obsidian,
            commands::ops::export_project,
            commands::ops::export_notes,
            commands::ops::get_autostart,
            commands::ops::set_autostart
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    // Clean MCP shutdown on app exit; stdio transports also kill their
    // children on drop as a safety net.
    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            if let Some(state) = app_handle.try_state::<AppState>() {
                lock_recover(&state.mcp).shutdown();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::commands::with_db;
    use super::*;

    // S1.4: a panic while a thread holds the DB mutex must not cascade into
    // every later command: with_db recovers the poisoned lock and still runs
    // the closure against the live connection (WAL + busy_timeout keep the
    // recovered SQLite connection safe to reuse).
    #[test]
    fn with_db_recovers_after_the_db_mutex_is_poisoned() {
        let wd = onboarding::temp_test_dir("poison-with-db");
        let state = AppState::new();
        *lock_recover(&state.db) = Some(db::open(&paths::db_path(&wd)).unwrap());
        let db = &state.db;
        std::thread::scope(|s| {
            let panicker = s.spawn(|| {
                let _guard = db.lock().unwrap();
                panic!("boom while holding the DB lock");
            });
            assert!(panicker.join().is_err());
        });
        assert!(state.db.is_poisoned());
        let tasks = with_db(&state, |conn| {
            let n: i64 = conn.query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))?;
            Ok(n)
        })
        .unwrap();
        assert_eq!(tasks, 0);
        std::fs::remove_dir_all(&wd).ok();
    }
}

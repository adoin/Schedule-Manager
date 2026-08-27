use crate::{
    DEFAULT_API_BASE_URL,
    api_client::ApiClient,
    calendar::{
        ScheduleKind, event_occurs_on, is_legal_rest_day, lunar_date_label, lunar_festival,
        lunar_label, lunar_parts, occurrence_start, schedule_kind, yearly_lunar_parts,
    },
    models::{CalendarEvent, Holiday},
    repository::LocalRepository,
};
use anyhow::{Context, Result, anyhow};
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::Asia::Shanghai;
use serde::{Deserialize, Serialize};
use slint::winit_030::WinitWindowAccessor;
use slint::{CloseRequestResponse, Color, ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
#[cfg(target_os = "windows")]
use std::process::{Command, Output, Stdio};
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicIsize, Ordering};
#[cfg(target_os = "macos")]
use std::{cell::RefCell, rc::Rc, time::Instant};
use std::{
    collections::{HashMap, VecDeque},
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::Duration as StdDuration,
};
slint::include_modules!();

static APPLICATION_LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn application_log_path() -> Option<PathBuf> {
    let directories = directories::ProjectDirs::from("com", "Emssion", "ScheduleManager")?;
    Some(directories.data_local_dir().join("logs").join(format!(
        "schedule-manager.{}.log",
        Local::now().format("%Y-%m-%d")
    )))
}

fn app_log(message: impl AsRef<str>) {
    let Some(path) = application_log_path() else {
        return;
    };
    let lock = APPLICATION_LOG_LOCK.get_or_init(|| Mutex::new(()));
    let Ok(_guard) = lock.lock() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(
        file,
        "{} pid={} {}",
        Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        std::process::id(),
        message.as_ref()
    );
}

fn install_application_panic_log() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        app_log(format!("panic: {panic_info}"));
        original_hook(panic_info);
    }));
}

struct UiState {
    visible_month: NaiveDate,
    selected_date: NaiveDate,
    selected_event_id: Option<String>,
    calendar_view: i32,
    token: Option<String>,
    email: Option<String>,
    sync_conflicts: VecDeque<SyncConflict>,
    widget_preview_event_id: Option<String>,
    pending_sync_cursor: Option<i64>,
    sync_in_progress: bool,
    desktop_drag_origin: Option<DesktopDragOrigin>,
}

#[derive(Clone, Copy)]
struct DesktopDragOrigin {
    cursor: (i32, i32),
    window: (i32, i32),
}

#[derive(Clone)]
struct SyncConflict {
    local: CalendarEvent,
    remote: Option<CalendarEvent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DesktopWidgetConfig {
    x: Option<i32>,
    y: Option<i32>,
    locked: bool,
}

#[cfg(target_os = "windows")]
#[derive(Debug, Deserialize)]
struct ExternalWidgetCommand {
    action: String,
    event_id: Option<String>,
    #[allow(dead_code)]
    created_at: i64,
}

impl Default for DesktopWidgetConfig {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            locked: true,
        }
    }
}

pub fn run() -> Result<()> {
    install_application_panic_log();
    app_log(format!(
        "application starting version={} exe={} args={:?}",
        env!("CARGO_PKG_VERSION"),
        std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| format!("<unavailable: {error}>")),
        std::env::args_os().collect::<Vec<_>>()
    ));
    let mut startup_hidden = std::env::args_os().any(|arg| arg == OsStr::new("--startup-hidden"));
    #[cfg(target_os = "windows")]
    let notification_identity_error = crate::windows_notifications::prepare_identity().err();
    let repository = LocalRepository::open()?;
    let today = Local::now().date_naive();
    let email = repository.setting("account_email")?;
    let token = email.as_deref().and_then(load_token);
    let app = AppWindow::new()?;
    let widget = DesktopWidget::new()?;
    let forced_reminder = ForcedReminderWindow::new()?;
    configure_forced_reminder_window(&forced_reminder);
    let widget_config = load_desktop_widget_config().unwrap_or_default();
    widget.set_locked(widget_config.locked);
    let close_behavior = repository
        .setting("close_behavior")?
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|value| (0..=3).contains(value))
        .unwrap_or(-1);
    let notifications_enabled = bool_setting(&repository, "notifications_enabled", true);
    let notification_sound_enabled = bool_setting(&repository, "notification_sound_enabled", true);
    let autostart_enabled = bool_setting(&repository, "autostart_enabled", false);
    let autostart_result = configure_autostart(autostart_enabled);
    app.set_close_behavior(close_behavior);
    app.set_settings_close_behavior(close_behavior.max(0));
    app.set_notifications_enabled(notifications_enabled);
    app.set_notification_sound_enabled(notification_sound_enabled);
    app.set_settings_notifications_enabled(notifications_enabled);
    app.set_settings_notification_sound_enabled(notification_sound_enabled);
    app.set_autostart_enabled(autostart_enabled);
    app.set_settings_autostart_enabled(autostart_enabled);
    app.set_autostart_status(
        autostart_status(autostart_enabled, autostart_result.as_ref().err()).into(),
    );
    let state = Arc::new(Mutex::new(UiState {
        visible_month: today.with_day(1).unwrap(),
        selected_date: today,
        selected_event_id: None,
        calendar_view: 1,
        token,
        email,
        sync_conflicts: VecDeque::new(),
        widget_preview_event_id: None,
        pending_sync_cursor: None,
        sync_in_progress: false,
        desktop_drag_origin: None,
    }));
    render_all_surfaces(&app, &widget, &state)?;
    #[cfg(target_os = "windows")]
    if let Some(error) = notification_identity_error {
        app.set_status(format!("Windows 通知身份注册失败，将使用兼容模式：{error}").into());
    }
    wire_callbacks(&app, &widget, &forced_reminder, state.clone());
    wire_desktop_widget_callbacks(&app, &widget, state.clone());
    wire_forced_reminder_callbacks(&app, &forced_reminder);
    let _widget_position_timer = start_desktop_widget_position_timer(&widget);
    let _widget_refresh_timer = start_desktop_widget_refresh_timer(&app, &widget, state.clone());
    #[cfg(target_os = "windows")]
    let _external_widget_command_timer =
        start_external_widget_command_timer(&app, &widget, state.clone());
    let _system_tray = match install_system_tray(&app, &widget, state.clone()) {
        Ok(tray) => Some(tray),
        Err(error) => {
            eprintln!("system tray startup failed: {error:#}");
            app_log(format!("system tray startup failed: {error:#}"));
            startup_hidden = false;
            app.set_close_behavior(-1);
            app.set_status(format!("系统托盘启动失败，请关闭时选择完全退出：{error}").into());
            None
        }
    };
    start_reminder_timer(&app, &forced_reminder);
    start_sync_timer(&app, state.clone());
    refresh_holidays_in_background(&app, state.clone());
    if state
        .lock()
        .ok()
        .and_then(|value| value.token.clone())
        .is_some()
    {
        sync_in_background(&app, state.clone());
    }
    if !startup_hidden {
        app.show()?;
    }
    app_log(format!(
        "event loop starting startup_hidden={startup_hidden}"
    ));
    slint::run_event_loop_until_quit()?;
    app_log("event loop stopped");
    Ok(())
}

fn wire_callbacks(
    app: &AppWindow,
    widget: &DesktopWidget,
    forced_reminder: &ForcedReminderWindow,
    state: Arc<Mutex<UiState>>,
) {
    let weak = app.as_weak();
    app.on_choose_close_action(move |behavior| {
        let Some(app) = weak.upgrade() else { return };
        if !(0..=3).contains(&behavior) {
            return;
        }
        match LocalRepository::open()
            .and_then(|repo| repo.set_setting("close_behavior", &behavior.to_string()))
        {
            Ok(()) => app.set_close_behavior(behavior),
            Err(error) => app.set_status(format!("保存关闭行为失败：{error}").into()),
        }
    });

    let weak = app.as_weak();
    let weak_widget = widget.as_weak();
    app.on_request_hide_to_tray(move || {
        if let Some(app) = weak.upgrade() {
            app_log("main window requested hide to tray");
            hide_main_window(&app, weak_widget.upgrade().as_ref());
        }
    });

    let weak_app = app.as_weak();
    let weak_widget = widget.as_weak();
    let shared = state.clone();
    app.on_request_dock_to_desktop(move || {
        let (Some(app), Some(widget)) = (weak_app.upgrade(), weak_widget.upgrade()) else {
            return;
        };
        if let Err(error) = dock_to_desktop(&app, &widget, &shared) {
            app_log(format!("dock to desktop failed: {error:#}"));
            app.set_status(format!("吸附到桌面失败：{error}").into());
        }
    });

    let weak = app.as_weak();
    let weak_forced_reminder = forced_reminder.as_weak();
    let shared = state.clone();
    app.on_request_full_exit(move || {
        let Some(app) = weak.upgrade() else { return };
        if app.get_exit_pending() {
            return;
        }
        let pending_forced = LocalRepository::open()
            .and_then(|repo| repo.pending_forced_reminders())
            .unwrap_or_default();
        if !pending_forced.is_empty() {
            if let Some(forced_reminder) = weak_forced_reminder.upgrade() {
                let _ = refresh_forced_reminder_window(&forced_reminder);
            }
            app.set_status("仍有强制提醒未处理，请先在提醒窗口点击“已处理”".into());
            return;
        }
        app.set_exit_pending(true);
        #[cfg(target_os = "windows")]
        close_external_desktop_widget();
        app_log("full exit requested; starting final sync");
        app.set_status("正在同步数据后退出…".into());
        let token = shared.lock().ok().and_then(|state| state.token.clone());
        let weak = app.as_weak();
        thread::spawn(move || {
            let sync_result = token.map(check_sync_consistency).transpose().map(|_| ());
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                if let Err(error) = sync_result {
                    app_log(format!("final sync failed: {error:#}"));
                    app.set_status(format!("退出前同步失败：{error}").into());
                }
                app_log("final sync completed; quitting event loop");
                let _ = mark_watcher_intentional_exit();
                let _ = app.hide();
                let _ = slint::quit_event_loop();
            });
        });
    });

    let weak = app.as_weak();
    app.on_save_settings(move || {
        let Some(app) = weak.upgrade() else { return };
        let close_behavior = app.get_settings_close_behavior().clamp(0, 3);
        let notifications_enabled = app.get_settings_notifications_enabled();
        let notification_sound_enabled = app.get_settings_notification_sound_enabled();
        let autostart_enabled = app.get_settings_autostart_enabled();
        let result = (|| -> Result<()> {
            configure_autostart(autostart_enabled)?;
            let repo = LocalRepository::open()?;
            repo.set_setting("close_behavior", &close_behavior.to_string())?;
            repo.set_setting(
                "notifications_enabled",
                if notifications_enabled {
                    "true"
                } else {
                    "false"
                },
            )?;
            repo.set_setting(
                "notification_sound_enabled",
                if notification_sound_enabled {
                    "true"
                } else {
                    "false"
                },
            )?;
            repo.set_setting(
                "autostart_enabled",
                if autostart_enabled { "true" } else { "false" },
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                app.set_close_behavior(close_behavior);
                app.set_notifications_enabled(notifications_enabled);
                app.set_notification_sound_enabled(notification_sound_enabled);
                app.set_autostart_enabled(autostart_enabled);
                app.set_autostart_status(autostart_status(autostart_enabled, None).into());
                app.set_settings_visible(false);
                app.set_status("设置已保存".into());
            }
            Err(error) => app.set_status(format!("保存设置失败：{error}").into()),
        }
    });

    let weak = app.as_weak();
    app.on_test_notification(move || {
        let Some(app) = weak.upgrade() else { return };
        if !app.get_settings_notifications_enabled() {
            app.set_status("请先启用系统通知".into());
            return;
        }
        match show_system_notification(
            "Schedule Manager 测试通知",
            "系统通知工作正常，日程提醒也会以这种方式显示。",
            app.get_settings_notification_sound_enabled(),
        ) {
            Ok(()) => app.set_status("测试通知已发送".into()),
            Err(error) => app.set_status(format!("测试通知失败：{error}").into()),
        }
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_previous_month(move || {
        if let Ok(mut state) = shared.lock() {
            state.visible_month = shift_month(state.visible_month, -1);
            state.calendar_view = 1;
        }
        if let Some(app) = weak.upgrade() {
            let _ = render_all(&app, &shared);
        }
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_next_month(move || {
        if let Ok(mut state) = shared.lock() {
            state.visible_month = shift_month(state.visible_month, 1);
            state.calendar_view = 1;
        }
        if let Some(app) = weak.upgrade() {
            let _ = render_all(&app, &shared);
        }
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_go_today(move || {
        let today = Local::now().date_naive();
        if let Ok(mut state) = shared.lock() {
            state.visible_month = today.with_day(1).unwrap();
            state.selected_date = today;
            state.selected_event_id = None;
            state.calendar_view = 0;
        }
        if let Some(app) = weak.upgrade() {
            app.set_editor_visible(false);
            let _ = render_all(&app, &shared);
        }
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_select_calendar_view(move |view| {
        if !matches!(view, 1..=3) {
            return;
        }
        if let Ok(mut state) = shared.lock() {
            state.calendar_view = view;
            state.selected_event_id = None;
        }
        if let Some(app) = weak.upgrade() {
            app.set_editor_visible(false);
            let _ = render_all(&app, &shared);
        }
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_select_day(move |value| {
        if let Ok(date) = NaiveDate::parse_from_str(&value, "%Y-%m-%d") {
            if let Ok(mut state) = shared.lock() {
                state.selected_date = date;
                state.selected_event_id = None;
                state.calendar_view = 1;
                if date.month() != state.visible_month.month()
                    || date.year() != state.visible_month.year()
                {
                    state.visible_month = date.with_day(1).unwrap();
                }
            }
        }
        if let Some(app) = weak.upgrade() {
            app.set_editor_visible(false);
            let _ = render_all(&app, &shared);
        }
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_select_event(move |id| {
        if let Ok(mut state) = shared.lock() {
            state.selected_event_id = Some(id.to_string());
        }
        if let Some(app) = weak.upgrade() {
            match load_event_into_editor(&app, &id) {
                Ok(()) => app.set_editor_visible(true),
                Err(error) => app.set_status(format!("读取日程失败：{error}").into()),
            }
        }
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_new_event(move || {
        if let Ok(mut state) = shared.lock() {
            state.selected_event_id = None;
        }
        if let Some(app) = weak.upgrade() {
            let date = shared
                .lock()
                .map(|state| state.selected_date)
                .unwrap_or_else(|_| Local::now().date_naive());
            clear_editor(&app, date);
            app.set_editor_visible(true);
        }
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_save_event(move || {
        let Some(app) = weak.upgrade() else { return };
        if app.get_event_saving() {
            return;
        }
        app.set_editor_feedback("".into());
        app.set_editor_feedback_error(false);
        match event_from_editor(&app, &shared) {
            Ok(event) => {
                let token = shared.lock().ok().and_then(|state| state.token.clone());
                if let Some(token) = token {
                    app.set_event_saving(true);
                    app.set_editor_feedback("正在保存到云端…".into());
                    app.set_status("正在保存到云端…".into());
                    let weak = app.as_weak();
                    let shared_thread = shared.clone();
                    thread::spawn(move || {
                        let result = api_client(Some(token))
                            .and_then(|client| client.upsert_event(&event))
                            .and_then(|remote| {
                                LocalRepository::open()?.upsert_event(&remote)?;
                                Ok(remote)
                            });
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(app) = weak.upgrade() else { return };
                            app.set_event_saving(false);
                            match result {
                                Ok(remote) => {
                                    if let Ok(mut state) = shared_thread.lock() {
                                        state.selected_event_id = Some(remote.id);
                                    }
                                    app.set_editor_visible(false);
                                    app.set_status("日程已保存，云端与本地一致".into());
                                    let _ = render_all(&app, &shared_thread);
                                }
                                Err(error) => {
                                    let message = format!("云端保存失败，本地未修改：{error}");
                                    app.set_editor_feedback(message.clone().into());
                                    app.set_editor_feedback_error(true);
                                    app.set_status(message.into());
                                }
                            }
                        });
                    });
                } else {
                    app.set_event_saving(true);
                    match LocalRepository::open().and_then(|repo| repo.upsert_event(&event)) {
                        Ok(()) => {
                            if let Ok(mut state) = shared.lock() {
                                state.selected_event_id = Some(event.id);
                            }
                            app.set_event_saving(false);
                            app.set_editor_visible(false);
                            app.set_status("日程已保存到本机，登录后可选择同步".into());
                            let _ = render_all(&app, &shared);
                        }
                        Err(error) => {
                            app.set_event_saving(false);
                            let message = format!("保存失败：{error}");
                            app.set_editor_feedback(message.clone().into());
                            app.set_editor_feedback_error(true);
                            app.set_status(message.into());
                        }
                    }
                }
            }
            Err(error) => {
                let message = format!("无法保存：{error}");
                app.set_editor_feedback(message.clone().into());
                app.set_editor_feedback_error(true);
                app.set_status(message.into());
            }
        }
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_delete_event(move || {
        let Some(app) = weak.upgrade() else { return };
        let id = shared
            .lock()
            .ok()
            .and_then(|state| state.selected_event_id.clone());
        let Some(id) = id else { return };
        let token = shared.lock().ok().and_then(|state| state.token.clone());
        if let Some(token) = token {
            let event = match LocalRepository::open().and_then(|repo| repo.event(&id)) {
                Ok(Some(event)) => event,
                Ok(None) => {
                    app.set_status("日程不存在".into());
                    return;
                }
                Err(error) => {
                    app.set_status(format!("读取日程失败：{error}").into());
                    return;
                }
            };
            app.set_status("正在从云端删除…".into());
            let weak = app.as_weak();
            let shared_thread = shared.clone();
            thread::spawn(move || {
                let result = api_client(Some(token))
                    .and_then(|client| client.delete_event(&event.id, event.version))
                    .and_then(|remote| {
                        LocalRepository::open()?.upsert_event(&remote)?;
                        Ok(())
                    });
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = weak.upgrade() else { return };
                    match result {
                        Ok(()) => {
                            if let Ok(mut state) = shared_thread.lock() {
                                state.selected_event_id = None;
                            }
                            app.set_editor_visible(false);
                            app.set_status("日程已从云端与本地删除".into());
                            let _ = render_all(&app, &shared_thread);
                        }
                        Err(error) => {
                            app.set_status(format!("云端删除失败，本地仍保留：{error}").into())
                        }
                    }
                });
            });
        } else {
            match LocalRepository::open().and_then(|repo| repo.mark_deleted(&id)) {
                Ok(Some(_)) => {
                    if let Ok(mut state) = shared.lock() {
                        state.selected_event_id = None;
                    }
                    app.set_editor_visible(false);
                    app.set_status("日程已从本机删除，登录后可选择同步".into());
                    let _ = render_all(&app, &shared);
                }
                Ok(None) => app.set_status("日程不存在".into()),
                Err(error) => app.set_status(format!("删除失败：{error}").into()),
            }
        }
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_resolve_sync_conflict(move |use_local| {
        let Some(app) = weak.upgrade() else { return };
        let (token, conflict) = match shared.lock() {
            Ok(state) => (state.token.clone(), state.sync_conflicts.front().cloned()),
            Err(_) => (None, None),
        };
        let Some(token) = token else {
            app.set_status("登录状态已失效，请重新登录".into());
            return;
        };
        let Some(conflict) = conflict else {
            app.set_sync_conflict_visible(false);
            return;
        };
        app.set_sync_conflict_visible(false);
        app.set_status(if use_local {
            "正在将本地版本写入云端…".into()
        } else {
            "正在采用云端版本…".into()
        });
        let weak = app.as_weak();
        let shared_thread = shared.clone();
        thread::spawn(move || {
            let result = resolve_sync_conflict(&token, &conflict, use_local);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                match result {
                    Ok(()) => {
                        let (has_more, cursor) = match shared_thread.lock() {
                            Ok(mut state) => {
                                if state
                                    .sync_conflicts
                                    .front()
                                    .is_some_and(|item| item.local.id == conflict.local.id)
                                {
                                    state.sync_conflicts.pop_front();
                                }
                                (
                                    !state.sync_conflicts.is_empty(),
                                    if state.sync_conflicts.is_empty() {
                                        state.pending_sync_cursor.take()
                                    } else {
                                        None
                                    },
                                )
                            }
                            Err(_) => (false, None),
                        };
                        if let Some(cursor) = cursor {
                            if let Ok(repo) = LocalRepository::open() {
                                let _ = repo.set_setting("sync_cursor", &cursor.to_string());
                            }
                        }
                        let _ = render_all(&app, &shared_thread);
                        if has_more {
                            show_current_sync_conflict(&app, &shared_thread);
                        } else {
                            app.set_sync_conflict_visible(false);
                            app.set_status("本地与云端已保持一致".into());
                        }
                    }
                    Err(error) => {
                        app.set_status(format!("处理数据差异失败：{error}").into());
                        show_current_sync_conflict(&app, &shared_thread);
                    }
                }
            });
        });
    });

    let weak = app.as_weak();
    app.on_show_account(move || {
        if let Some(app) = weak.upgrade() {
            app.set_auth_visible(true);
            app.set_auth_message("".into());
        }
    });

    let weak = app.as_weak();
    app.on_request_code(move || {
        let Some(app) = weak.upgrade() else { return };
        let email = app.get_auth_email().to_string();
        app.set_auth_message("正在发送验证码…".into());
        let weak = app.as_weak();
        thread::spawn(move || {
            let result = api_client(None).and_then(|client| client.request_code(&email));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = weak.upgrade() {
                    app.set_auth_message(match result {
                        Ok(()) => "验证码已发送，请查收邮箱".into(),
                        Err(error) => format!("发送失败：{error}").into(),
                    });
                }
            });
        });
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_submit_auth(move || {
        let Some(app) = weak.upgrade() else { return };
        let register = app.get_register_mode();
        let email = app.get_auth_email().to_string();
        let password = app.get_auth_password().to_string();
        let display_name = app.get_auth_display_name().to_string();
        let code = app.get_auth_code().to_string();
        app.set_auth_message(if register {
            "正在注册…".into()
        } else {
            "正在登录…".into()
        });
        let weak = app.as_weak();
        let shared_thread = shared.clone();
        thread::spawn(move || {
            let result = api_client(None).and_then(|client| {
                if register {
                    client.register(&email, &password, &display_name, &code)
                } else {
                    client.login(&email, &password)
                }
            });
            match result {
                Ok(auth) => {
                    let _ = save_token(&auth.user.email, &auth.token);
                    if let Ok(repo) = LocalRepository::open() {
                        let _ = repo.set_setting("account_email", &auth.user.email);
                    }
                    if let Ok(mut state) = shared_thread.lock() {
                        state.token = Some(auth.token);
                        state.email = Some(auth.user.email.clone());
                        state.sync_conflicts.clear();
                        state.pending_sync_cursor = None;
                        state.sync_in_progress = false;
                    }
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = weak.upgrade() {
                            app.set_auth_visible(false);
                            app.set_auth_password("".into());
                            app.set_status("登录成功，正在同步".into());
                            let _ = render_all(&app, &shared_thread);
                            sync_in_background(&app, shared_thread.clone());
                        }
                    });
                }
                Err(error) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = weak.upgrade() {
                            app.set_auth_message(format!("操作失败：{error}").into());
                        }
                    });
                }
            }
        });
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_logout(move || {
        let email = shared.lock().ok().and_then(|state| state.email.clone());
        if let Some(email) = email {
            let _ = delete_token(&email);
        }
        if let Ok(repo) = LocalRepository::open() {
            let _ = repo.set_setting("account_email", "");
        }
        if let Ok(mut state) = shared.lock() {
            state.token = None;
            state.email = None;
            state.sync_conflicts.clear();
            state.pending_sync_cursor = None;
            state.sync_in_progress = false;
        }
        if let Some(app) = weak.upgrade() {
            app.set_auth_visible(false);
            app.set_sync_conflict_visible(false);
            app.set_status("已退出账户，本机日程仍保留".into());
            let _ = render_all(&app, &shared);
        }
    });

    let weak = app.as_weak();
    app.on_change_schedule_kind(move |value| {
        if let Some(app) = weak.upgrade() {
            let kind = ScheduleKind::from_index(value);
            app.set_event_schedule_kind(kind.index());
            app.set_event_schedule_label(kind.label().into());
            if !matches!(kind, ScheduleKind::Specific | ScheduleKind::YearlySolar) {
                app.set_event_all_day(false);
            }
        }
    });

    let weak = app.as_weak();
    app.on_adjust_event_date(move |part, delta| {
        if let Some(app) = weak.upgrade() {
            if let Ok(current) = NaiveDate::parse_from_str(&app.get_event_date(), "%Y-%m-%d") {
                set_editor_date(&app, adjust_date(current, part, delta));
            }
        }
    });

    let weak = app.as_weak();
    app.on_adjust_event_time(move |part, delta| {
        if let Some(app) = weak.upgrade() {
            let hour = app
                .get_event_hour_input()
                .trim()
                .parse::<i32>()
                .unwrap_or(app.get_event_hour())
                .clamp(0, 23);
            let minute = app
                .get_event_minute_input()
                .trim()
                .parse::<i32>()
                .unwrap_or(app.get_event_minute())
                .clamp(0, 59);
            let total = hour * 60 + minute;
            let step = if part == 0 { 60 } else { 5 };
            let adjusted = (total + delta * step).rem_euclid(24 * 60);
            set_editor_time(&app, adjusted / 60, adjusted % 60);
        }
    });

    let weak = app.as_weak();
    app.on_apply_event_time(move |hour, minute| {
        let Some(app) = weak.upgrade() else {
            return false;
        };
        let Ok(hour) = hour.trim().parse::<i32>() else {
            app.set_status("小时必须是 0–23 的整数".into());
            return false;
        };
        let Ok(minute) = minute.trim().parse::<i32>() else {
            app.set_status("分钟必须是 0–59 的整数".into());
            return false;
        };
        if !(0..=23).contains(&hour) {
            app.set_status("小时必须在 0–23 之间".into());
            return false;
        }
        if !(0..=59).contains(&minute) {
            app.set_status("分钟必须在 0–59 之间".into());
            return false;
        }
        set_editor_time(&app, hour, minute);
        true
    });

    let weak = app.as_weak();
    app.on_adjust_lunar_date(move |part, delta| {
        if let Some(app) = weak.upgrade() {
            let mut month = app.get_event_lunar_month().clamp(1, 12);
            let mut day = app.get_event_lunar_day().clamp(1, 30);
            if part == 0 {
                month = (month - 1 + delta).rem_euclid(12) + 1;
            } else {
                day = (day - 1 + delta).rem_euclid(30) + 1;
            }
            app.set_event_lunar_month(month);
            app.set_event_lunar_day(day);
            app.set_event_lunar_date(
                lunar_date_label(month as u8, day as u8, app.get_event_lunar_leap()).into(),
            );
        }
    });

    let weak = app.as_weak();
    app.on_start_window_drag(move || {
        if let Some(app) = weak.upgrade() {
            begin_system_window_drag(&app);
        }
    });
}

fn wire_desktop_widget_callbacks(
    app: &AppWindow,
    widget: &DesktopWidget,
    state: Arc<Mutex<UiState>>,
) {
    let weak_app = app.as_weak();
    let weak_widget = widget.as_weak();
    let shared = state.clone();
    widget.on_previous_month(move || {
        if let Ok(mut state) = shared.lock() {
            state.visible_month = shift_month(state.visible_month, -1);
            state.widget_preview_event_id = None;
        }
        if let (Some(app), Some(widget)) = (weak_app.upgrade(), weak_widget.upgrade()) {
            let _ = render_all_surfaces(&app, &widget, &shared);
        }
    });

    let weak_app = app.as_weak();
    let weak_widget = widget.as_weak();
    let shared = state.clone();
    widget.on_next_month(move || {
        if let Ok(mut state) = shared.lock() {
            state.visible_month = shift_month(state.visible_month, 1);
            state.widget_preview_event_id = None;
        }
        if let (Some(app), Some(widget)) = (weak_app.upgrade(), weak_widget.upgrade()) {
            let _ = render_all_surfaces(&app, &widget, &shared);
        }
    });

    let weak_app = app.as_weak();
    let weak_widget = widget.as_weak();
    let shared = state.clone();
    widget.on_go_today(move || {
        let today = Local::now().date_naive();
        if let Ok(mut state) = shared.lock() {
            state.visible_month = today.with_day(1).unwrap();
            state.selected_date = today;
            state.widget_preview_event_id = None;
        }
        if let (Some(app), Some(widget)) = (weak_app.upgrade(), weak_widget.upgrade()) {
            let _ = render_all_surfaces(&app, &widget, &shared);
        }
    });

    let weak_app = app.as_weak();
    let weak_widget = widget.as_weak();
    let shared = state.clone();
    widget.on_select_day(move |value| {
        if let Ok(date) = NaiveDate::parse_from_str(&value, "%Y-%m-%d")
            && let Ok(mut state) = shared.lock()
        {
            state.selected_date = date;
            state.widget_preview_event_id = None;
            if date.month() != state.visible_month.month()
                || date.year() != state.visible_month.year()
            {
                state.visible_month = date.with_day(1).unwrap();
            }
        }
        if let (Some(app), Some(widget)) = (weak_app.upgrade(), weak_widget.upgrade()) {
            let _ = render_all_surfaces(&app, &widget, &shared);
        }
    });

    let weak_app = app.as_weak();
    let weak_widget = widget.as_weak();
    let shared = state.clone();
    widget.on_preview_event(move |id| {
        if let Ok(mut state) = shared.lock() {
            state.widget_preview_event_id = Some(id.to_string());
        }
        if let (Some(app), Some(widget)) = (weak_app.upgrade(), weak_widget.upgrade()) {
            let _ = render_all_surfaces(&app, &widget, &shared);
        }
    });

    let weak_app = app.as_weak();
    let weak_widget = widget.as_weak();
    let shared = state.clone();
    widget.on_edit_event(move |id| {
        let (Some(app), Some(widget)) = (weak_app.upgrade(), weak_widget.upgrade()) else {
            return;
        };
        if let Ok(mut state) = shared.lock() {
            state.selected_event_id = Some(id.to_string());
        }
        match load_event_into_editor(&app, &id) {
            Ok(()) => {
                app.set_editor_visible(true);
                show_main_window(&app, Some(&widget));
            }
            Err(error) => app.set_status(format!("读取日程失败：{error}").into()),
        }
    });

    let weak_widget = widget.as_weak();
    widget.on_set_locked(move |locked| {
        let Some(widget) = weak_widget.upgrade() else {
            return;
        };
        widget.set_locked(locked);
        app_log(format!("desktop widget locked={locked}"));
        let mut config = load_desktop_widget_config().unwrap_or_default();
        config.locked = locked;
        if let Some((x, y)) = desktop_widget_position(&widget) {
            config.x = Some(x);
            config.y = Some(y);
        }
        let _ = save_desktop_widget_config(&config);
    });

    let weak_widget = widget.as_weak();
    let shared = state.clone();
    widget.on_start_drag(move || {
        if let Some(widget) = weak_widget.upgrade()
            && !widget.get_locked()
        {
            app_log("desktop widget drag requested");
            let origin = begin_desktop_widget_drag(&widget);
            if let Ok(mut state) = shared.lock() {
                state.desktop_drag_origin = origin;
            }
        }
    });

    let weak_widget = widget.as_weak();
    let shared = state.clone();
    widget.on_drag_move(move || {
        let Some(widget) = weak_widget.upgrade() else {
            return;
        };
        let origin = shared
            .lock()
            .ok()
            .and_then(|state| state.desktop_drag_origin);
        if let Some(origin) = origin {
            continue_desktop_widget_drag(&widget, origin);
        }
    });

    let weak_widget = widget.as_weak();
    let shared = state.clone();
    widget.on_end_drag(move || {
        let was_dragging = shared
            .lock()
            .map(|mut state| state.desktop_drag_origin.take().is_some())
            .unwrap_or(false);
        if was_dragging {
            app_log("desktop widget drag ended");
            if let Some(widget) = weak_widget.upgrade() {
                apply_desktop_widget_window_shape(&widget);
            }
        }
    });

    let weak_app = app.as_weak();
    let weak_widget = widget.as_weak();
    widget.on_open_main(move || {
        if let (Some(app), Some(widget)) = (weak_app.upgrade(), weak_widget.upgrade()) {
            app_log("desktop widget requested main window");
            show_main_window(&app, Some(&widget));
        }
    });

    let weak_app = app.as_weak();
    let weak_widget = widget.as_weak();
    widget.on_hide_widget(move || {
        if let Some(widget) = weak_widget.upgrade() {
            app_log("desktop widget requested hide to tray");
            let _ = widget.hide();
        }
        if let Some(app) = weak_app.upgrade() {
            app.set_status("桌面日历已隐藏到系统托盘".into());
        }
    });
}

fn render_all_surfaces(
    app: &AppWindow,
    widget: &DesktopWidget,
    state: &Arc<Mutex<UiState>>,
) -> Result<()> {
    render_all(app, state)?;
    render_desktop_widget(app, widget, state)
}

fn render_desktop_widget(
    app: &AppWindow,
    widget: &DesktopWidget,
    state: &Arc<Mutex<UiState>>,
) -> Result<()> {
    widget.set_calendar_weeks(app.get_calendar_weeks());
    widget.set_selected_events(app.get_selected_events());
    widget.set_month_title(app.get_month_title());
    widget.set_selected_date_title(app.get_selected_date_title());

    let preview_id = state
        .lock()
        .map_err(|_| anyhow!("UI state unavailable"))?
        .widget_preview_event_id
        .clone();
    let Some(preview_id) = preview_id else {
        clear_desktop_widget_preview(widget);
        return Ok(());
    };
    let Some(event) = LocalRepository::open()?.event(&preview_id)? else {
        clear_desktop_widget_preview(widget);
        return Ok(());
    };
    let start = event.start_at.with_timezone(&Shanghai);
    let time = if event.all_day {
        "全天".to_owned()
    } else {
        start.format("%H:%M").to_string()
    };
    let detail = [
        event.location.trim(),
        event.notes.trim(),
        reminder_summary(&event.reminder_minutes).as_str(),
    ]
    .into_iter()
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>()
    .join(" · ");
    widget.set_preview_event_id(preview_id.into());
    widget.set_preview_title(event.title.into());
    widget.set_preview_time(time.into());
    widget.set_preview_detail(if detail.is_empty() {
        "没有额外备注".into()
    } else {
        detail.into()
    });
    Ok(())
}

fn clear_desktop_widget_preview(widget: &DesktopWidget) {
    widget.set_preview_event_id("".into());
    widget.set_preview_title("点击日程查看详情".into());
    widget.set_preview_time("".into());
    widget.set_preview_detail("".into());
}

fn render_all(app: &AppWindow, state: &Arc<Mutex<UiState>>) -> Result<()> {
    let state_guard = state.lock().map_err(|_| anyhow!("UI state unavailable"))?;
    let repository = LocalRepository::open()?;
    let month = state_guard.visible_month;
    let selected = state_guard.selected_date;
    let grid_start = month - Duration::days(month.weekday().num_days_from_monday() as i64);
    let grid_end = grid_start + Duration::days(42);
    let events = repository.active_events()?;
    let holidays = repository.holidays_between(
        &grid_start.to_string(),
        &(grid_end - Duration::days(1)).to_string(),
    )?;
    let holiday_map: HashMap<String, Holiday> = holidays
        .into_iter()
        .map(|holiday| (holiday.date.clone(), holiday))
        .collect();
    let today = Local::now().date_naive();
    let mut cells = Vec::with_capacity(42);
    for offset in 0..42 {
        let date = grid_start + Duration::days(offset);
        let holiday = holiday_map.get(&date.to_string());
        let day_events: Vec<&CalendarEvent> = events
            .iter()
            .filter(|event| event_occurs_on(event, date, holiday))
            .collect();
        let event_text = day_events
            .iter()
            .take(3)
            .map(|event| format!("• {}", event.title))
            .collect::<Vec<_>>()
            .join("\n");
        let festival = lunar_festival(date).unwrap_or("");
        let holiday_name = holiday
            .map(|item| item.name.as_str())
            .filter(|value| !value.is_empty())
            .unwrap_or(festival);
        let off_day = is_legal_rest_day(date, holiday);
        cells.push(DayCell {
            date: date.to_string().into(),
            day: date.day().to_string().into(),
            lunar: lunar_label(date).into(),
            holiday: holiday_name.into(),
            events: event_text.into(),
            in_month: date.month() == month.month(),
            today: date == today,
            selected: date == selected,
            off_day,
            workday: holiday.is_some_and(|item| !item.is_off_day),
            day_kind: if holiday.is_some_and(|item| !item.is_off_day) {
                "班".into()
            } else if off_day {
                "休".into()
            } else {
                "".into()
            },
        });
    }
    let selected_events = events
        .iter()
        .filter(|event| event_occurs_on(event, selected, holiday_map.get(&selected.to_string())))
        .map(|event| event_row(event, selected))
        .collect::<Vec<_>>();
    let overview_events = events
        .iter()
        .filter(|event| state_guard.calendar_view != 3 || event.completed)
        .map(overview_event_row)
        .collect::<Vec<_>>();
    let weeks = cells
        .chunks_exact(7)
        .map(|days| CalendarWeek {
            d0: days[0].clone(),
            d1: days[1].clone(),
            d2: days[2].clone(),
            d3: days[3].clone(),
            d4: days[4].clone(),
            d5: days[5].clone(),
            d6: days[6].clone(),
        })
        .collect::<Vec<_>>();
    app.set_calendar_weeks(ModelRc::new(VecModel::from(weeks)));
    app.set_selected_events(ModelRc::new(VecModel::from(selected_events)));
    app.set_overview_events(ModelRc::new(VecModel::from(overview_events.clone())));
    app.set_calendar_view(state_guard.calendar_view);
    app.set_overview_title(
        if state_guard.calendar_view == 3 {
            "已完成"
        } else {
            "全部日程"
        }
        .into(),
    );
    app.set_overview_empty(
        if state_guard.calendar_view == 3 {
            "还没有已完成的日程"
        } else {
            "还没有日程，点击左上角新建"
        }
        .into(),
    );
    app.set_month_title(format!("{}年 {}月", month.year(), month.month()).into());
    app.set_selected_date_title(
        format!(
            "{} · {}",
            selected.format("%m月%d日"),
            weekday_name(selected.weekday())
        )
        .into(),
    );
    app.set_signed_in(state_guard.token.is_some());
    app.set_user_label(state_guard.email.as_deref().unwrap_or("登录 / 注册").into());
    app.set_selected_event_id(
        state_guard
            .selected_event_id
            .clone()
            .unwrap_or_default()
            .into(),
    );
    Ok(())
}

fn event_row(event: &CalendarEvent, occurrence_date: NaiveDate) -> EventRow {
    let local = occurrence_start(event, occurrence_date)
        .unwrap_or(event.start_at)
        .with_timezone(&Shanghai);
    let time = if event.all_day {
        "全天".into()
    } else {
        local.format("%H:%M").to_string()
    };
    let recurrence = schedule_kind(event);
    let schedule_summary = if recurrence == ScheduleKind::Specific {
        ""
    } else {
        recurrence.label()
    };
    let meta = [
        schedule_summary,
        event.location.as_str(),
        reminder_summary(&event.reminder_minutes).as_str(),
    ]
    .into_iter()
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>()
    .join(" · ");
    EventRow {
        id: event.id.clone().into(),
        date: occurrence_date.format("%m月%d日").to_string().into(),
        time: time.into(),
        title: event.title.clone().into(),
        meta: meta.into(),
        color: parse_color(&event.color),
        completed: event.completed,
    }
}

fn overview_event_row(event: &CalendarEvent) -> EventRow {
    let local = event.start_at.with_timezone(&Shanghai);
    let kind = schedule_kind(event);
    let date = match kind {
        ScheduleKind::Specific => local.format("%Y-%m-%d").to_string(),
        ScheduleKind::LegalRestDay => "法定休息日".to_string(),
        ScheduleKind::LegalWorkday => "法定工作日".to_string(),
        ScheduleKind::YearlySolar => local.format("每年 %m-%d").to_string(),
        ScheduleKind::YearlyLunar => {
            let (month, day, leap) = yearly_lunar_parts(event);
            format!("每年 {}", lunar_date_label(month, day, leap))
        }
        ScheduleKind::Daily => "每天".to_string(),
    };
    let time = if event.all_day {
        "全天".to_string()
    } else {
        local.format("%H:%M").to_string()
    };
    let mut meta = Vec::new();
    if kind != ScheduleKind::Specific {
        meta.push(kind.label().to_string());
    }
    if !event.location.trim().is_empty() {
        meta.push(event.location.clone());
    }
    let reminder = reminder_summary(&event.reminder_minutes);
    if !reminder.is_empty() {
        meta.push(reminder);
    }
    if event.force_reminder {
        meta.push("强制提醒".into());
    }
    EventRow {
        id: event.id.clone().into(),
        date: date.into(),
        time: time.into(),
        title: event.title.clone().into(),
        meta: meta.join(" · ").into(),
        color: parse_color(&event.color),
        completed: event.completed,
    }
}

fn load_event_into_editor(app: &AppWindow, id: &str) -> Result<()> {
    let event = LocalRepository::open()?
        .event(id)?
        .context("event not found")?;
    let start = event.start_at.with_timezone(&Shanghai);
    let duration = (event.end_at - event.start_at).num_minutes().max(1);
    let kind = schedule_kind(&event);
    let (lunar_month, lunar_day, lunar_leap) = yearly_lunar_parts(&event);
    app.set_event_title(event.title.into());
    app.set_event_schedule_kind(kind.index());
    app.set_event_schedule_label(kind.label().into());
    set_editor_date(app, start.date_naive());
    set_editor_time(app, start.hour() as i32, start.minute() as i32);
    app.set_event_duration(duration.to_string().into());
    app.set_event_location(event.location.into());
    app.set_event_notes(event.notes.into());
    app.set_event_reminders(
        event
            .reminder_minutes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
            .into(),
    );
    app.set_event_lunar_month(lunar_month as i32);
    app.set_event_lunar_day(lunar_day as i32);
    app.set_event_lunar_leap(lunar_leap);
    app.set_event_lunar_date(lunar_date_label(lunar_month, lunar_day, lunar_leap).into());
    app.set_reminder_at_time(event.reminder_minutes.contains(&0));
    app.set_reminder_10m(event.reminder_minutes.contains(&10));
    app.set_reminder_1h(event.reminder_minutes.contains(&60));
    app.set_reminder_1d(event.reminder_minutes.contains(&1_440));
    app.set_event_custom_reminder(
        event
            .reminder_minutes
            .iter()
            .find(|value| !matches!(**value, 0 | 10 | 60 | 1_440))
            .map(ToString::to_string)
            .unwrap_or_default()
            .into(),
    );
    app.set_reminder_custom_applied(
        event
            .reminder_minutes
            .iter()
            .any(|value| !matches!(*value, 0 | 10 | 60 | 1_440)),
    );
    app.set_event_force_reminder(event.force_reminder);
    app.set_event_all_day(event.all_day);
    app.set_event_completed(event.completed);
    app.set_event_saving(false);
    app.set_editor_feedback("".into());
    app.set_editor_feedback_error(false);
    Ok(())
}

fn clear_editor(app: &AppWindow, date: NaiveDate) {
    let next_hour = (Local::now().hour() + 1).min(23);
    app.set_selected_event_id("".into());
    app.set_event_title("".into());
    app.set_event_schedule_kind(ScheduleKind::Specific.index());
    app.set_event_schedule_label(ScheduleKind::Specific.label().into());
    set_editor_date(app, date);
    set_editor_time(app, next_hour as i32, 0);
    app.set_event_duration("60".into());
    app.set_event_location("".into());
    app.set_event_notes("".into());
    app.set_event_reminders("0".into());
    let (month, day, leap) = lunar_parts(date).unwrap_or((1, 1, false));
    app.set_event_lunar_month(month as i32);
    app.set_event_lunar_day(day as i32);
    app.set_event_lunar_leap(leap);
    app.set_event_lunar_date(lunar_date_label(month, day, leap).into());
    app.set_reminder_at_time(true);
    app.set_reminder_10m(false);
    app.set_reminder_1h(false);
    app.set_reminder_1d(false);
    app.set_event_custom_reminder("".into());
    app.set_reminder_custom_applied(false);
    app.set_event_force_reminder(false);
    app.set_event_all_day(false);
    app.set_event_completed(false);
    app.set_event_saving(false);
    app.set_editor_feedback("".into());
    app.set_editor_feedback_error(false);
}

fn event_from_editor(app: &AppWindow, state: &Arc<Mutex<UiState>>) -> Result<CalendarEvent> {
    let title = app.get_event_title().trim().to_string();
    if title.is_empty() {
        return Err(anyhow!("标题不能为空"));
    }
    let kind = ScheduleKind::from_index(app.get_event_schedule_kind());
    let date = NaiveDate::parse_from_str(&app.get_event_date(), "%Y-%m-%d")
        .context("日期格式应为 YYYY-MM-DD")?;
    let all_day = app.get_event_all_day()
        && matches!(kind, ScheduleKind::Specific | ScheduleKind::YearlySolar);
    let time = if all_day {
        NaiveTime::from_hms_opt(0, 0, 0).unwrap()
    } else {
        NaiveTime::parse_from_str(&app.get_event_start_time(), "%H:%M")
            .context("开始时间格式应为 HH:mm")?
    };
    let duration: i64 = app
        .get_event_duration()
        .trim()
        .parse()
        .context("时长必须是分钟数")?;
    if duration <= 0 || duration > 525_600 {
        return Err(anyhow!("时长必须在 1 分钟到 1 年之间"));
    }
    let start_local = Shanghai
        .from_local_datetime(&date.and_time(time))
        .single()
        .context("该本地时间无效")?;
    let start = start_local.with_timezone(&Utc);
    let mut reminders = Vec::new();
    if app.get_reminder_at_time() {
        reminders.push(0);
    }
    if app.get_reminder_10m() {
        reminders.push(10);
    }
    if app.get_reminder_1h() {
        reminders.push(60);
    }
    if app.get_reminder_1d() {
        reminders.push(1_440);
    }
    let custom = app.get_event_custom_reminder();
    if app.get_reminder_custom_applied() && !custom.trim().is_empty() {
        let custom: i64 = custom.trim().parse().context("自定义提醒必须是分钟数")?;
        if !(0..=525_600).contains(&custom) {
            return Err(anyhow!("自定义提醒需在 0 分钟到 1 年之间"));
        }
        reminders.push(custom);
    }
    reminders.sort_unstable();
    reminders.dedup();
    let selected_id = state
        .lock()
        .ok()
        .and_then(|state| state.selected_event_id.clone());
    let mut event = if let Some(id) = selected_id {
        LocalRepository::open()?
            .event(&id)?
            .unwrap_or_else(|| CalendarEvent::draft(start, start + Duration::minutes(duration)))
    } else {
        CalendarEvent::draft(start, start + Duration::minutes(duration))
    };
    event.title = title;
    event.location = app.get_event_location().to_string();
    event.notes = app.get_event_notes().to_string();
    event.start_at = start;
    event.end_at = start + Duration::minutes(duration);
    event.all_day = all_day;
    event.recurrence_rule = match kind {
        ScheduleKind::Specific => String::new(),
        ScheduleKind::LegalRestDay => "LEGAL_REST_DAY".into(),
        ScheduleKind::LegalWorkday => "LEGAL_WORKDAY".into(),
        ScheduleKind::YearlySolar => format!("YEARLY_SOLAR:{:02}-{:02}", date.month(), date.day()),
        ScheduleKind::YearlyLunar => format!(
            "YEARLY_LUNAR:{:02}-{:02}:{}",
            app.get_event_lunar_month(),
            app.get_event_lunar_day(),
            if app.get_event_lunar_leap() {
                "leap"
            } else {
                "regular"
            }
        ),
        ScheduleKind::Daily => "DAILY".into(),
    };
    event.completed = app.get_event_completed();
    event.reminder_minutes = reminders;
    event.force_reminder = app.get_event_force_reminder();
    event.updated_seq = 0;
    event.updated_at = Utc::now();
    Ok(event)
}

fn sync_in_background(app: &AppWindow, state: Arc<Mutex<UiState>>) {
    let token = match state.lock() {
        Ok(mut state) => {
            if state.sync_in_progress || !state.sync_conflicts.is_empty() {
                return;
            }
            let token = state.token.clone();
            if token.is_some() {
                state.sync_in_progress = true;
            }
            token
        }
        Err(_) => None,
    };
    let Some(token) = token else {
        return;
    };
    let weak = app.as_weak();
    thread::spawn(move || {
        let result = check_sync_consistency(token);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                if let Ok(mut state) = state.lock() {
                    state.sync_in_progress = false;
                }
                match result {
                    Ok((conflicts, cursor)) => {
                        let has_conflicts = !conflicts.is_empty();
                        if let Ok(mut state) = state.lock() {
                            state.sync_conflicts = conflicts;
                            state.pending_sync_cursor = has_conflicts.then_some(cursor);
                        }
                        let _ = render_all(&app, &state);
                        if has_conflicts {
                            app.set_status("检测到本地与云端差异，请选择采用哪一份".into());
                            show_current_sync_conflict(&app, &state);
                        } else {
                            app.set_sync_conflict_visible(false);
                            app.set_status("本地与云端已保持一致".into());
                        }
                    }
                    Err(error) => app.set_status(format!("同步失败：{error}").into()),
                }
            }
        });
    });
}

fn check_sync_consistency(token: String) -> Result<(VecDeque<SyncConflict>, i64)> {
    let client = api_client(Some(token))?;
    let pull = client.pull(0)?;
    let mut repo = LocalRepository::open()?;
    let mut remote_by_id = pull
        .events
        .iter()
        .cloned()
        .map(|event| (event.id.clone(), event))
        .collect::<HashMap<_, _>>();
    let mut conflicts = VecDeque::new();
    for local in repo.all_events()? {
        match remote_by_id.remove(&local.id) {
            Some(remote) if event_content_equal(&local, &remote) => {
                repo.upsert_event(&remote)?;
            }
            Some(remote) => conflicts.push_back(SyncConflict {
                local,
                remote: Some(remote),
            }),
            None if local.deleted => repo.purge_event(&local.id)?,
            None => conflicts.push_back(SyncConflict {
                local,
                remote: None,
            }),
        }
    }
    for remote in remote_by_id.into_values() {
        repo.upsert_event(&remote)?;
    }
    repo.replace_holidays(&pull.holidays)?;
    if conflicts.is_empty() {
        repo.set_setting("sync_cursor", &pull.cursor.to_string())?;
    }
    Ok((conflicts, pull.cursor))
}

fn resolve_sync_conflict(token: &str, conflict: &SyncConflict, use_local: bool) -> Result<()> {
    let repo = LocalRepository::open()?;
    if use_local {
        if conflict.local.deleted {
            if let Some(remote) = &conflict.remote {
                let deleted = api_client(Some(token.to_string()))?
                    .delete_event(&remote.id, remote.version)?;
                repo.upsert_event(&deleted)?;
            } else {
                repo.purge_event(&conflict.local.id)?;
            }
        } else {
            let mut local = conflict.local.clone();
            if let Some(remote) = &conflict.remote {
                local.version = remote.version;
            }
            let saved = api_client(Some(token.to_string()))?.upsert_event(&local)?;
            repo.upsert_event(&saved)?;
        }
    } else if let Some(remote) = &conflict.remote {
        repo.upsert_event(remote)?;
    } else {
        repo.purge_event(&conflict.local.id)?;
    }
    Ok(())
}

fn show_current_sync_conflict(app: &AppWindow, state: &Arc<Mutex<UiState>>) {
    let conflict = state
        .lock()
        .ok()
        .and_then(|state| state.sync_conflicts.front().cloned());
    let Some(conflict) = conflict else {
        app.set_sync_conflict_visible(false);
        return;
    };
    app.set_sync_conflict_title(conflict.local.title.clone().into());
    app.set_sync_conflict_local(sync_event_summary(Some(&conflict.local)).into());
    app.set_sync_conflict_cloud(sync_event_summary(conflict.remote.as_ref()).into());
    app.set_sync_conflict_visible(true);
}

fn sync_event_summary(event: Option<&CalendarEvent>) -> String {
    let Some(event) = event else {
        return "云端不存在这条日程".into();
    };
    let start = event.start_at.with_timezone(&Shanghai);
    format!(
        "{}\n{}\n{}\n提醒：{}",
        if event.deleted {
            "已删除"
        } else {
            &event.title
        },
        start.format("%Y-%m-%d %H:%M"),
        if event.location.is_empty() {
            "未填写地点"
        } else {
            &event.location
        },
        format!(
            "{}{}",
            reminder_summary(&event.reminder_minutes),
            if event.force_reminder {
                "（强制提醒）"
            } else {
                ""
            }
        )
    )
}

fn event_content_equal(left: &CalendarEvent, right: &CalendarEvent) -> bool {
    left.title == right.title
        && left.notes == right.notes
        && left.location == right.location
        && left.start_at == right.start_at
        && left.end_at == right.end_at
        && left.timezone == right.timezone
        && left.all_day == right.all_day
        && left.color == right.color
        && left.recurrence_rule == right.recurrence_rule
        && left.reminder_minutes == right.reminder_minutes
        && left.force_reminder == right.force_reminder
        && left.completed == right.completed
        && left.deleted == right.deleted
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    #[test]
    fn sync_comparison_ignores_server_metadata() {
        let start = Utc::now();
        let left = CalendarEvent::draft(start, start + Duration::hours(1));
        let mut right = left.clone();
        right.version = 8;
        right.updated_seq = 42;
        right.updated_at = right.updated_at + Duration::minutes(3);
        assert!(event_content_equal(&left, &right));
    }

    #[test]
    fn sync_comparison_detects_user_data_changes() {
        let start = Utc::now();
        let left = CalendarEvent::draft(start, start + Duration::hours(1));
        let mut right = left.clone();
        right.title = "云端标题".into();
        assert!(!event_content_equal(&left, &right));

        let mut right = left.clone();
        right.force_reminder = true;
        assert!(!event_content_equal(&left, &right));
    }
}

fn refresh_holidays_in_background(app: &AppWindow, state: Arc<Mutex<UiState>>) {
    let year = Local::now().year();
    let weak = app.as_weak();
    thread::spawn(move || {
        let result = (|| -> Result<usize> {
            let holidays = api_client(None)?.holidays(year - 1, year + 1)?;
            let count = holidays.len();
            let mut repository = LocalRepository::open()?;
            repository.replace_holidays(&holidays)?;
            Ok(count)
        })();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                match result {
                    Ok(count) => {
                        app.set_status(format!("法定节假日已更新 · {count} 条").into());
                        let _ = render_all(&app, &state);
                    }
                    Err(error) => {
                        app.set_status(format!("使用本地节假日缓存：{error}").into());
                    }
                }
            }
        });
    });
}

fn start_sync_timer(app: &AppWindow, state: Arc<Mutex<UiState>>) {
    let timer = Timer::default();
    let weak = app.as_weak();
    timer.start(
        TimerMode::Repeated,
        StdDuration::from_secs(5 * 60),
        move || {
            if let Some(app) = weak.upgrade() {
                sync_in_background(&app, state.clone());
            }
        },
    );
    std::mem::forget(timer);
}

fn configure_forced_reminder_window(reminder: &ForcedReminderWindow) {
    reminder
        .window()
        .on_close_requested(|| CloseRequestResponse::KeepWindowShown);
    #[cfg(target_os = "windows")]
    let _ = reminder
        .window()
        .with_winit_window(configure_windows_forced_reminder_window);
}

#[cfg(target_os = "windows")]
static FORCED_REMINDER_ORIGINAL_WNDPROC: AtomicIsize = AtomicIsize::new(0);

#[cfg(target_os = "windows")]
unsafe extern "system" fn forced_reminder_frameless_wndproc(
    window: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, DefWindowProcW, GWL_EXSTYLE, GWL_STYLE, STYLESTRUCT, WM_NCACTIVATE,
        WM_NCCALCSIZE, WM_NCPAINT, WM_STYLECHANGING, WNDPROC, WS_BORDER, WS_CAPTION, WS_CHILD,
        WS_DLGFRAME, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_EX_NOACTIVATE, WS_EX_STATICEDGE,
        WS_EX_TOOLWINDOW, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU,
        WS_THICKFRAME,
    };

    match message {
        WM_NCCALCSIZE | WM_NCPAINT => return 0,
        WM_NCACTIVATE => return 1,
        WM_STYLECHANGING if lparam != 0 => unsafe {
            let style = &mut *(lparam as *mut STYLESTRUCT);
            if wparam as i32 == GWL_STYLE {
                let frame = WS_CHILD
                    | WS_CAPTION
                    | WS_THICKFRAME
                    | WS_BORDER
                    | WS_DLGFRAME
                    | WS_MINIMIZEBOX
                    | WS_MAXIMIZEBOX
                    | WS_SYSMENU;
                style.styleNew = (style.styleNew & !frame) | WS_POPUP;
            } else if wparam as i32 == GWL_EXSTYLE {
                let frame =
                    WS_EX_DLGMODALFRAME | WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE;
                style.styleNew = (style.styleNew & !frame) | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
            }
        },
        _ => {}
    }

    let original = FORCED_REMINDER_ORIGINAL_WNDPROC.load(Ordering::Acquire);
    if original == 0 {
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    } else {
        let procedure: WNDPROC = unsafe { std::mem::transmute(original) };
        unsafe { CallWindowProcW(procedure, window, message, wparam, lparam) }
    }
}

#[cfg(target_os = "windows")]
unsafe fn install_forced_reminder_frameless_wndproc(window: windows_sys::Win32::Foundation::HWND) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GWLP_WNDPROC, SetWindowLongPtrW};

    if FORCED_REMINDER_ORIGINAL_WNDPROC.load(Ordering::Acquire) != 0 {
        return;
    }
    let previous = unsafe {
        SetWindowLongPtrW(
            window,
            GWLP_WNDPROC,
            forced_reminder_frameless_wndproc as *const () as isize,
        )
    };
    if previous != 0 {
        FORCED_REMINDER_ORIGINAL_WNDPROC.store(previous, Ordering::Release);
        app_log(format!(
            "forced reminder frameless wndproc installed hwnd={window:p}"
        ));
    } else {
        app_log(format!(
            "forced reminder frameless wndproc installation failed hwnd={window:p}"
        ));
    }
}

#[cfg(target_os = "windows")]
fn configure_windows_forced_reminder_window(window: &slint::winit_030::winit::window::Window) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::{
        Dwm::{
            DWMNCRP_DISABLED, DWMWA_BORDER_COLOR, DWMWA_NCRENDERING_POLICY,
            DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
        },
        Gdi::{CreateRoundRectRgn, DeleteObject, SetWindowRgn},
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GWL_STYLE, GetClientRect, GetWindowLongW, HWND_TOPMOST, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowLongW, SetWindowPos, WS_BORDER,
        WS_CAPTION, WS_CHILD, WS_DLGFRAME, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_EX_NOACTIVATE,
        WS_EX_STATICEDGE, WS_EX_TOOLWINDOW, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX,
        WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
    };

    use slint::winit_030::winit::platform::windows::WindowExtWindows;
    window.set_decorations(false);
    window.set_skip_taskbar(true);

    let Some(hwnd) = windows_window_handle(window) else {
        return;
    };
    unsafe {
        install_forced_reminder_frameless_wndproc(hwnd);
        let style_before = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let frame_style = WS_CHILD
            | WS_CAPTION
            | WS_THICKFRAME
            | WS_BORDER
            | WS_DLGFRAME
            | WS_MINIMIZEBOX
            | WS_MAXIMIZEBOX
            | WS_SYSMENU;
        let style_after = (style_before & !frame_style) | WS_POPUP;
        let ex_style_before = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let frame_ex_style =
            WS_EX_DLGMODALFRAME | WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE;
        let ex_style_after =
            (ex_style_before & !frame_ex_style) | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
        if style_before != style_after {
            SetWindowLongW(hwnd, GWL_STYLE, style_after as i32);
        }
        if ex_style_before != ex_style_after {
            SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style_after as i32);
        }

        let nc_policy = DWMNCRP_DISABLED;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY as u32,
            &nc_policy as *const i32 as _,
            std::mem::size_of_val(&nc_policy) as u32,
        );
        let corner_preference = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &corner_preference as *const i32 as _,
            std::mem::size_of_val(&corner_preference) as u32,
        );
        let border_color = 0xffff_fffeu32;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            &border_color as *const u32 as _,
            std::mem::size_of_val(&border_color) as u32,
        );

        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        let mut client = RECT::default();
        if GetClientRect(hwnd, &mut client) != 0 && client.right > 0 && client.bottom > 0 {
            let region = CreateRoundRectRgn(0, 0, client.right, client.bottom, 36, 36);
            if !region.is_null() && SetWindowRgn(hwnd, region, 1) == 0 {
                DeleteObject(region);
            }
        }
    }
}

fn position_forced_reminder_window(reminder: &ForcedReminderWindow) {
    use slint::winit_030::winit::dpi::PhysicalPosition;

    let _ = reminder.window().with_winit_window(|window| {
        let Some(monitor) = window
            .current_monitor()
            .or_else(|| window.available_monitors().next())
        else {
            return;
        };
        let monitor_position = monitor.position();
        let monitor_size = monitor.size();
        let window_size = window.outer_size();
        let margin = 24i32;
        let x = monitor_position.x + monitor_size.width.saturating_sub(window_size.width) as i32
            - margin;
        let y = monitor_position.y + margin;
        window.set_outer_position(PhysicalPosition::new(x, y));
        #[cfg(target_os = "windows")]
        configure_windows_forced_reminder_window(window);
    });
}

fn refresh_forced_reminder_window(reminder: &ForcedReminderWindow) -> Result<bool> {
    let pending = LocalRepository::open()?.pending_forced_reminders()?;
    let Some(current) = pending.first() else {
        reminder.hide()?;
        return Ok(false);
    };
    reminder.set_reminder_key(current.key.clone().into());
    reminder.set_reminder_title(current.title.clone().into());
    reminder.set_reminder_body(current.body.clone().into());
    reminder.set_pending_count(pending.len().min(i32::MAX as usize) as i32);
    if !reminder.window().is_visible() {
        reminder.show()?;
        position_forced_reminder_window(reminder);
        app_log(format!(
            "forced reminder shown key={} pending_count={}",
            current.key,
            pending.len()
        ));
    }
    #[cfg(target_os = "windows")]
    {
        use slint::winit_030::WinitWindowAccessor;
        let _ = reminder
            .window()
            .with_winit_window(configure_windows_forced_reminder_window);
    }
    Ok(true)
}

fn wire_forced_reminder_callbacks(app: &AppWindow, reminder: &ForcedReminderWindow) {
    let drag_origin = Arc::new(Mutex::new(None::<DesktopDragOrigin>));
    let weak_reminder = reminder.as_weak();
    let drag_origin_for_start = drag_origin.clone();
    reminder.on_start_drag(move || {
        let Some(reminder) = weak_reminder.upgrade() else {
            return;
        };
        let origin = begin_forced_reminder_drag(&reminder);
        if let Ok(mut current) = drag_origin_for_start.lock() {
            *current = origin;
        }
    });

    let weak_reminder = reminder.as_weak();
    let drag_origin_for_move = drag_origin.clone();
    reminder.on_drag_move(move || {
        let Some(reminder) = weak_reminder.upgrade() else {
            return;
        };
        let origin = drag_origin_for_move.lock().ok().and_then(|value| *value);
        if let Some(origin) = origin {
            continue_forced_reminder_drag(&reminder, origin);
        }
    });

    reminder.on_end_drag(move || {
        if let Ok(mut current) = drag_origin.lock() {
            *current = None;
        }
    });

    let weak_app = app.as_weak();
    let weak_reminder = reminder.as_weak();
    reminder.on_acknowledged(move || {
        let (Some(app), Some(reminder)) = (weak_app.upgrade(), weak_reminder.upgrade()) else {
            return;
        };
        let key = reminder.get_reminder_key().to_string();
        let result = (|| -> Result<bool> {
            if key.is_empty() {
                return refresh_forced_reminder_window(&reminder);
            }
            LocalRepository::open()?.acknowledge_forced_reminder(&key)?;
            app_log(format!("forced reminder acknowledged key={key}"));
            refresh_forced_reminder_window(&reminder)
        })();
        match result {
            Ok(true) => app.set_status("已处理一条强制提醒，正在显示下一条".into()),
            Ok(false) => app.set_status("强制提醒已处理".into()),
            Err(error) => app.set_status(format!("处理强制提醒失败：{error}").into()),
        }
    });
}

#[cfg(target_os = "windows")]
fn forced_reminder_window_position(reminder: &ForcedReminderWindow) -> Option<(i32, i32)> {
    use slint::winit_030::WinitWindowAccessor;
    reminder
        .window()
        .with_winit_window(|window| {
            window
                .outer_position()
                .ok()
                .map(|position| (position.x, position.y))
        })
        .flatten()
}

#[cfg(target_os = "windows")]
fn begin_forced_reminder_drag(reminder: &ForcedReminderWindow) -> Option<DesktopDragOrigin> {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};

    let window = forced_reminder_window_position(reminder)?;
    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) } == 0 {
        app_log("forced reminder drag failed: cursor position unavailable");
        return None;
    }
    Some(DesktopDragOrigin {
        cursor: (cursor.x, cursor.y),
        window,
    })
}

#[cfg(target_os = "macos")]
fn begin_forced_reminder_drag(reminder: &ForcedReminderWindow) -> Option<DesktopDragOrigin> {
    use slint::winit_030::WinitWindowAccessor;
    let _ = reminder
        .window()
        .with_winit_window(|window| window.drag_window());
    None
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn begin_forced_reminder_drag(_reminder: &ForcedReminderWindow) -> Option<DesktopDragOrigin> {
    None
}

#[cfg(target_os = "windows")]
fn continue_forced_reminder_drag(reminder: &ForcedReminderWindow, origin: DesktopDragOrigin) {
    use slint::winit_030::{WinitWindowAccessor, winit::dpi::PhysicalPosition};
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};

    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) } == 0 {
        return;
    }
    let x = origin.window.0 + cursor.x - origin.cursor.0;
    let y = origin.window.1 + cursor.y - origin.cursor.1;
    let _ = reminder
        .window()
        .with_winit_window(|window| window.set_outer_position(PhysicalPosition::new(x, y)));
}

#[cfg(not(target_os = "windows"))]
fn continue_forced_reminder_drag(_reminder: &ForcedReminderWindow, _origin: DesktopDragOrigin) {}

fn start_reminder_timer(app: &AppWindow, forced_reminder: &ForcedReminderWindow) {
    if let Err(error) = refresh_forced_reminder_window(forced_reminder) {
        app.set_status(format!("恢复强制提醒失败：{error}").into());
    }
    deliver_due_reminders(app, forced_reminder);
    let timer = Timer::default();
    let weak = app.as_weak();
    let weak_forced_reminder = forced_reminder.as_weak();
    timer.start(TimerMode::Repeated, StdDuration::from_secs(30), move || {
        let (Some(app), Some(forced_reminder)) = (weak.upgrade(), weak_forced_reminder.upgrade())
        else {
            return;
        };
        let _ = refresh_forced_reminder_window(&forced_reminder);
        deliver_due_reminders(&app, &forced_reminder);
    });
    std::mem::forget(timer);
}

fn deliver_due_reminders(app: &AppWindow, forced_reminder: &ForcedReminderWindow) {
    let Ok(repo) = LocalRepository::open() else {
        return;
    };
    if !bool_setting(&repo, "notifications_enabled", true) {
        return;
    }
    let sound_enabled = bool_setting(&repo, "notification_sound_enabled", true);
    let now = Utc::now();
    let Ok(due) = repo.due_reminders(now) else {
        return;
    };
    for (event, occurrence, offset) in due {
        let local = occurrence.with_timezone(&Shanghai);
        let when = if local.date_naive() == Local::now().date_naive() {
            local.format("今天 %H:%M").to_string()
        } else {
            local.format("%m月%d日 %H:%M").to_string()
        };
        let body = if offset == 0 {
            format!("{when} 开始")
        } else {
            format!("{when} 开始 · 提前{}提醒", human_offset(offset))
        };
        if event.force_reminder {
            let queued = repo
                .enqueue_forced_reminder(&event, occurrence, offset, &body, now)
                .and_then(|_| {
                    repo.mark_reminder_delivered(&event.id, event.version, occurrence, offset, now)
                });
            match queued {
                Ok(()) => {
                    if let Err(error) = refresh_forced_reminder_window(forced_reminder) {
                        app.set_status(
                            format!("显示强制提醒失败，将在下次启动恢复：{error}").into(),
                        );
                    } else {
                        let _ = show_system_notification(&event.title, &body, sound_enabled);
                        app.set_status(format!("强制提醒：{} · {}", event.title, body).into());
                    }
                }
                Err(error) => {
                    app.set_status(format!("记录强制提醒失败，将自动重试：{error}").into())
                }
            }
            continue;
        }
        match show_system_notification(&event.title, &body, sound_enabled) {
            Ok(()) => match repo.mark_reminder_delivered(
                &event.id,
                event.version,
                occurrence,
                offset,
                now,
            ) {
                Ok(()) => app.set_status(format!("提醒：{} · {}", event.title, body).into()),
                Err(error) => app.set_status(format!("记录提醒投递失败：{error}").into()),
            },
            Err(error) => app.set_status(format!("系统通知发送失败，将自动重试：{error}").into()),
        }
    }
}

fn show_system_notification(title: &str, body: &str, sound_enabled: bool) -> Result<()> {
    let mut notification = notify_rust::Notification::new();
    notification.summary(title).body(body);
    #[cfg(target_os = "windows")]
    {
        if crate::windows_notifications::prepare_identity().is_ok() {
            notification.app_id(crate::windows_notifications::APP_USER_MODEL_ID);
        }
    }
    #[cfg(target_os = "macos")]
    if sound_enabled {
        notification.sound_name("default");
    }
    let result = notification
        .show()
        .map(|_| ())
        .map_err(|error| anyhow!(error.to_string()));
    if result.is_ok() && sound_enabled {
        #[cfg(target_os = "windows")]
        crate::windows_notifications::play_reminder_sound();
    }
    result
}

fn bool_setting(repo: &LocalRepository, key: &str, default: bool) -> bool {
    repo.setting(key)
        .ok()
        .flatten()
        .and_then(|value| value.parse::<bool>().ok())
        .unwrap_or(default)
}

fn mark_watcher_intentional_exit() -> Result<()> {
    let Ok(path) = std::env::var("SCHEDULE_WATCHER_EXIT_SENTINEL") else {
        return Ok(());
    };
    let path = std::path::PathBuf::from(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, b"intentional")?;
    Ok(())
}

fn api_client(token: Option<String>) -> Result<ApiClient> {
    ApiClient::new(
        std::env::var("SCHEDULE_API_BASE_URL").unwrap_or_else(|_| DEFAULT_API_BASE_URL.into()),
        token,
    )
}

fn save_token(email: &str, token: &str) -> Result<()> {
    keyring::Entry::new("ScheduleManager", email)?.set_password(token)?;
    Ok(())
}

fn load_token(email: &str) -> Option<String> {
    keyring::Entry::new("ScheduleManager", email)
        .ok()?
        .get_password()
        .ok()
}

fn delete_token(email: &str) -> Result<()> {
    keyring::Entry::new("ScheduleManager", email)?.delete_credential()?;
    Ok(())
}

fn shift_month(month: NaiveDate, delta: i32) -> NaiveDate {
    let total = month.year() * 12 + month.month0() as i32 + delta;
    NaiveDate::from_ymd_opt(total.div_euclid(12), total.rem_euclid(12) as u32 + 1, 1).unwrap()
}

fn adjust_date(date: NaiveDate, part: i32, delta: i32) -> NaiveDate {
    match part {
        0 => {
            let year = (date.year() + delta).clamp(1900, 2100);
            let day = date.day().min(days_in_month(year, date.month()));
            NaiveDate::from_ymd_opt(year, date.month(), day).unwrap()
        }
        1 => {
            let first = date.with_day(1).unwrap();
            let shifted = shift_month(first, delta);
            let day = date
                .day()
                .min(days_in_month(shifted.year(), shifted.month()));
            shifted.with_day(day).unwrap()
        }
        _ => date
            .checked_add_signed(Duration::days(delta as i64))
            .filter(|value| (1900..=2100).contains(&value.year()))
            .unwrap_or(date),
    }
}

fn set_editor_date(app: &AppWindow, date: NaiveDate) {
    app.set_event_date(date.to_string().into());
    app.set_event_date_year(date.year());
    app.set_event_date_month(date.month() as i32);
    app.set_event_date_day(date.day() as i32);
}

fn set_editor_time(app: &AppWindow, hour: i32, minute: i32) {
    let hour = hour.rem_euclid(24);
    let minute = minute.rem_euclid(60);
    app.set_event_start_time(format!("{hour:02}:{minute:02}").into());
    app.set_event_hour(hour);
    app.set_event_minute(minute);
    app.set_event_hour_input(format!("{hour:02}").into());
    app.set_event_minute_input(format!("{minute:02}").into());
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1).unwrap()
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1).unwrap()
    };
    (next - Duration::days(1)).day()
}

fn weekday_name(value: Weekday) -> &'static str {
    match value {
        Weekday::Mon => "周一",
        Weekday::Tue => "周二",
        Weekday::Wed => "周三",
        Weekday::Thu => "周四",
        Weekday::Fri => "周五",
        Weekday::Sat => "周六",
        Weekday::Sun => "周日",
    }
}

fn reminder_summary(values: &[i64]) -> String {
    values
        .first()
        .map(|value| format!("提前{}", human_offset(*value)))
        .unwrap_or_default()
}

fn human_offset(minutes: i64) -> String {
    if minutes % 1440 == 0 {
        format!("{}天", minutes / 1440)
    } else if minutes % 60 == 0 {
        format!("{}小时", minutes / 60)
    } else {
        format!("{}分钟", minutes)
    }
}

fn parse_color(value: &str) -> Color {
    let value = value.trim_start_matches('#');
    if value.len() == 6 {
        if let Ok(rgb) = u32::from_str_radix(value, 16) {
            return Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
        }
    }
    Color::from_rgb_u8(104, 120, 214)
}

fn autostart_status(enabled: bool, error: Option<&anyhow::Error>) -> String {
    if let Some(error) = error {
        return format!("启动项检测失败：{error}");
    }
    if !enabled {
        return "未启用".to_owned();
    }
    #[cfg(target_os = "windows")]
    return "已启用：Windows 登录计划任务已检测并同步".to_owned();
    #[cfg(target_os = "macos")]
    return "已启用：macOS 用户启动项已检测并同步".to_owned();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    "当前系统不支持开机启动".to_owned()
}

#[cfg(target_os = "windows")]
const AUTOSTART_TASK_NAME: &str = "Schedule Manager";

#[cfg(target_os = "windows")]
fn configure_autostart(enabled: bool) -> Result<()> {
    if !enabled {
        if scheduled_task_exists()? {
            let output = run_schtasks(&["/Delete", "/TN", AUTOSTART_TASK_NAME, "/F"])?;
            ensure_command_succeeded("删除开机启动计划任务", &output)?;
        }
        return Ok(());
    }

    let executable = std::env::current_exe().context("无法读取当前程序路径")?;
    let action = format!("\"{}\" --startup-hidden", executable.display());
    let output = run_schtasks(&[
        "/Create",
        "/TN",
        AUTOSTART_TASK_NAME,
        "/TR",
        &action,
        "/SC",
        "ONLOGON",
        "/DELAY",
        "0000:10",
        "/RL",
        "LIMITED",
        "/F",
    ])?;
    ensure_command_succeeded("创建开机启动计划任务", &output)?;
    if !scheduled_task_exists()? {
        return Err(anyhow!("计划任务创建后未能通过检测"));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn scheduled_task_exists() -> Result<bool> {
    Ok(run_schtasks(&["/Query", "/TN", AUTOSTART_TASK_NAME])?
        .status
        .success())
}

#[cfg(target_os = "windows")]
fn run_schtasks(arguments: &[&str]) -> Result<Output> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    Command::new("schtasks.exe")
        .args(arguments)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("无法运行 Windows 计划任务工具")
}

#[cfg(target_os = "windows")]
fn ensure_command_succeeded(action: &str, output: &Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let detail = if output.stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout)
    } else {
        String::from_utf8_lossy(&output.stderr)
    };
    Err(anyhow!("{action}失败：{}", detail.trim()))
}

#[cfg(target_os = "macos")]
fn configure_autostart(enabled: bool) -> Result<()> {
    let launch_agent = launch_agent_path()?;
    if !enabled {
        if launch_agent.exists() {
            fs::remove_file(&launch_agent).context("无法删除 macOS 用户启动项")?;
        }
        return Ok(());
    }

    let executable = std::env::current_exe().context("无法读取当前程序路径")?;
    let parent = launch_agent
        .parent()
        .ok_or_else(|| anyhow!("macOS 用户启动项目录无效"))?;
    fs::create_dir_all(parent).context("无法创建 macOS 用户启动项目录")?;
    let executable = xml_escape(&executable.to_string_lossy());
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.emssion.schedule-manager</string>
  <key>ProgramArguments</key>
  <array><string>{executable}</string><string>--startup-hidden</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
</dict>
</plist>
"#
    );
    fs::write(&launch_agent, plist).context("无法写入 macOS 用户启动项")?;
    let installed = fs::read_to_string(&launch_agent).context("无法检测 macOS 用户启动项")?;
    if !installed.contains("--startup-hidden") || !installed.contains(&executable) {
        return Err(anyhow!("macOS 用户启动项写入后未能通过检测"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn launch_agent_path() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().ok_or_else(|| anyhow!("无法定位用户目录"))?;
    Ok(base
        .home_dir()
        .join("Library/LaunchAgents/com.emssion.schedule-manager.plist"))
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn configure_autostart(enabled: bool) -> Result<()> {
    if enabled {
        Err(anyhow!("当前系统不支持开机启动"))
    } else {
        Ok(())
    }
}

fn desktop_widget_config_path() -> Result<PathBuf> {
    let directories = directories::ProjectDirs::from("com", "Emssion", "ScheduleManager")
        .ok_or_else(|| anyhow!("无法定位本地配置目录"))?;
    Ok(directories.config_dir().join("desktop-widget.json"))
}

#[cfg(target_os = "windows")]
fn external_widget_command_path() -> Result<PathBuf> {
    let directories = directories::ProjectDirs::from("com", "Emssion", "ScheduleManager")
        .ok_or_else(|| anyhow!("无法定位本地数据目录"))?;
    Ok(directories.data_local_dir().join("widget-command.json"))
}

#[cfg(target_os = "windows")]
fn take_external_widget_command() -> Result<Option<ExternalWidgetCommand>> {
    let path = external_widget_command_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let payload =
        fs::read(&path).with_context(|| format!("无法读取桌面挂件命令：{}", path.display()))?;
    // Remove first so an invalid or interrupted command cannot be replayed forever.
    let _ = fs::remove_file(&path);
    let command = serde_json::from_slice(&payload)
        .with_context(|| format!("桌面挂件命令格式无效：{}", path.display()))?;
    Ok(Some(command))
}

#[cfg(target_os = "windows")]
fn start_external_widget_command_timer(
    app: &AppWindow,
    widget: &DesktopWidget,
    state: Arc<Mutex<UiState>>,
) -> Timer {
    // Do not replay a stale click from a widget that was closed before this launch.
    if let Ok(path) = external_widget_command_path() {
        let _ = fs::remove_file(path);
    }
    let weak_app = app.as_weak();
    let weak_widget = widget.as_weak();
    let timer = Timer::default();
    timer.start(
        TimerMode::Repeated,
        StdDuration::from_millis(150),
        move || {
            let Some(app) = weak_app.upgrade() else {
                return;
            };
            let Some(widget) = weak_widget.upgrade() else {
                return;
            };
            let command = match take_external_widget_command() {
                Ok(Some(command)) => command,
                Ok(None) => return,
                Err(error) => {
                    app_log(format!("external widget command failed: {error:#}"));
                    return;
                }
            };
            app_log(format!(
                "external widget command action={} event_id={:?}",
                command.action, command.event_id
            ));
            match command.action.as_str() {
                "open" => show_main_window(&app, Some(&widget)),
                "edit" => {
                    let Some(id) = command.event_id else {
                        return;
                    };
                    if let Ok(mut state) = state.lock() {
                        state.selected_event_id = Some(id.clone());
                    }
                    match load_event_into_editor(&app, &id) {
                        Ok(()) => {
                            app.set_editor_visible(true);
                            show_main_window(&app, Some(&widget));
                        }
                        Err(error) => {
                            show_main_window(&app, Some(&widget));
                            app.set_status(format!("读取日程失败：{error}").into());
                        }
                    }
                }
                action => app_log(format!("ignored external widget command action={action}")),
            }
        },
    );
    timer
}

#[cfg(target_os = "windows")]
fn external_desktop_widget_executable() -> Result<PathBuf> {
    let current = std::env::current_exe().context("无法定位主程序")?;
    let directory = current.parent().ok_or_else(|| anyhow!("主程序目录无效"))?;
    Ok(directory.join("schedule-desktop-widget.exe"))
}

#[cfg(target_os = "windows")]
fn external_desktop_widget_window() -> Option<windows_sys::Win32::Foundation::HWND> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;

    // This title is intentionally different from the legacy hidden Slint
    // component; matching the old title made docking skip the WGPU process.
    let title = OsStr::new("Schedule Manager Desktop Widget WGPU")
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let window = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    (!window.is_null()).then_some(window)
}

#[cfg(target_os = "windows")]
fn spawn_external_desktop_widget(app: &AppWindow) -> Result<()> {
    if external_desktop_widget_window().is_some() {
        app_log("external desktop widget already running");
        return Ok(());
    }
    let executable = external_desktop_widget_executable()?;
    if !executable.is_file() {
        return Err(anyhow!(
            "桌面挂件程序不存在：{}；请先运行 scripts/dev.ps1 重新构建",
            executable.display()
        ));
    }
    let log_path = application_log_path()
        .and_then(|path| path.parent().map(|parent| parent.to_path_buf()))
        .ok_or_else(|| anyhow!("无法定位挂件日志目录"))?
        .join(format!(
            "schedule-desktop-widget.{}.stderr.log",
            Local::now().format("%Y-%m-%d")
        ));
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).context("无法创建挂件日志目录")?;
    }
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("无法创建挂件错误日志：{}", log_path.display()))?;
    let stdout = stderr.try_clone().context("无法复制挂件日志句柄")?;
    let mut child = Command::new(&executable)
        .env("RUST_BACKTRACE", "full")
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| format!("无法启动桌面挂件：{}", executable.display()))?;
    let process_id = child.id();
    app_log(format!(
        "external desktop widget started pid={} exe={}",
        process_id,
        executable.display()
    ));
    let weak_app = app.as_weak();
    thread::spawn(move || match child.wait() {
        Ok(status) => {
            app_log(format!(
                "external desktop widget exited pid={process_id} status={status}"
            ));
            if !status.success() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = weak_app.upgrade() {
                        show_main_window(&app, None);
                        app.set_status(
                            format!("桌面挂件异常退出（{status}），主界面已自动恢复").into(),
                        );
                    }
                });
            }
        }
        Err(error) => {
            app_log(format!(
                "external desktop widget wait failed pid={process_id}: {error}"
            ));
        }
    });
    Ok(())
}

#[cfg(target_os = "windows")]
fn close_external_desktop_widget() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
    if let Some(window) = external_desktop_widget_window() {
        unsafe {
            let _ = PostMessageW(window, WM_CLOSE, 0, 0);
        }
        app_log("external desktop widget close requested");
    }
}

fn load_desktop_widget_config() -> Result<DesktopWidgetConfig> {
    let path = desktop_widget_config_path()?;
    if !path.exists() {
        return Ok(DesktopWidgetConfig::default());
    }
    let payload = fs::read_to_string(&path)
        .with_context(|| format!("无法读取桌面挂件配置：{}", path.display()))?;
    serde_json::from_str(&payload)
        .with_context(|| format!("桌面挂件配置格式无效：{}", path.display()))
}

fn save_desktop_widget_config(config: &DesktopWidgetConfig) -> Result<()> {
    let path = desktop_widget_config_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("桌面挂件配置目录无效"))?;
    fs::create_dir_all(parent).context("无法创建桌面挂件配置目录")?;
    let payload = serde_json::to_string_pretty(config)?;
    fs::write(&path, payload).with_context(|| format!("无法保存桌面挂件配置：{}", path.display()))
}

fn dock_to_desktop(
    app: &AppWindow,
    widget: &DesktopWidget,
    state: &Arc<Mutex<UiState>>,
) -> Result<()> {
    app_log("dock to desktop started");
    #[cfg(target_os = "windows")]
    {
        let _ = state;
        let _ = widget.hide();
        spawn_external_desktop_widget(app)?;
        app.set_status("日历已吸附到桌面；可从托盘恢复主程序".into());
        app.hide()?;
        app_log("main window hidden after external widget launch");
        return Ok(());
    }
    #[cfg(not(target_os = "windows"))]
    {
        render_all_surfaces(app, widget, state)?;
        widget.show()?;
        app_log("desktop widget show completed");
        configure_desktop_widget_window(widget);
        schedule_desktop_widget_window_configuration(widget);
        app.set_status("日历已吸附到桌面；可从托盘恢复主程序".into());
        app.hide()?;
        app_log("main window hidden after docking");
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
fn schedule_desktop_widget_window_configuration(widget: &DesktopWidget) {
    let weak = widget.as_weak();
    Timer::single_shot(StdDuration::from_millis(100), move || {
        if let Some(widget) = weak.upgrade() {
            app_log("desktop widget configuration retry delay_ms=100");
            configure_desktop_widget_window(&widget);
        }
    });
    let weak = widget.as_weak();
    Timer::single_shot(StdDuration::from_millis(750), move || {
        if let Some(widget) = weak.upgrade() {
            app_log("desktop widget configuration retry delay_ms=750");
            configure_desktop_widget_window(&widget);
        }
    });
}

#[cfg(not(target_os = "windows"))]
fn configure_desktop_widget_window(widget: &DesktopWidget) {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        use slint::winit_030::WinitWindowAccessor;
        #[cfg(target_os = "macos")]
        use slint::winit_030::winit::window::WindowLevel;

        let config = load_desktop_widget_config().unwrap_or_default();
        let configured = widget.window().with_winit_window(|window| {
            window.set_decorations(false);
            window.set_transparent(true);
            #[cfg(target_os = "macos")]
            window.set_window_level(WindowLevel::AlwaysOnBottom);
            #[cfg(target_os = "windows")]
            {
                use slint::winit_030::winit::platform::windows::WindowExtWindows;
                window.set_skip_taskbar(true);
                attach_windows_widget_to_desktop(window);
                apply_windows_widget_shape(window);
            }
            #[cfg(target_os = "macos")]
            if let Err(error) = window_vibrancy::apply_vibrancy(
                window,
                window_vibrancy::NSVisualEffectMaterial::HudWindow,
                None,
                Some(24.0),
            ) {
                eprintln!("desktop widget vibrancy failed: {error}");
            }

            let saved_position = config.x.zip(config.y).filter(|(x, y)| {
                window.available_monitors().any(|monitor| {
                    let position = monitor.position();
                    let size = monitor.size();
                    *x + 80 > position.x
                        && *y + 80 > position.y
                        && *x < position.x + size.width as i32
                        && *y < position.y + size.height as i32
                })
            });
            if let Some((x, y)) = saved_position {
                window
                    .set_outer_position(slint::winit_030::winit::dpi::PhysicalPosition::new(x, y));
            } else if let Some(monitor) = window.current_monitor() {
                let monitor_position = monitor.position();
                let monitor_size = monitor.size();
                let window_size = window.outer_size();
                let x =
                    monitor_position.x + monitor_size.width as i32 - window_size.width as i32 - 28;
                let y = monitor_position.y + 44;
                window
                    .set_outer_position(slint::winit_030::winit::dpi::PhysicalPosition::new(x, y));
            }
        });
        if configured.is_none() {
            app_log(format!(
                "desktop widget configuration skipped: winit window unavailable visible={}",
                widget.window().is_visible()
            ));
        } else {
            app_log("desktop widget configuration completed");
        }
    }
}

fn desktop_widget_position(widget: &DesktopWidget) -> Option<(i32, i32)> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        use slint::winit_030::WinitWindowAccessor;
        widget
            .window()
            .with_winit_window(|window| {
                window.outer_position().ok().map(|value| (value.x, value.y))
            })
            .flatten()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    None
}

fn start_desktop_widget_position_timer(widget: &DesktopWidget) -> Timer {
    let weak = widget.as_weak();
    let last_position = Arc::new(Mutex::new(None::<(i32, i32)>));
    let timer = Timer::default();
    timer.start(
        TimerMode::Repeated,
        StdDuration::from_millis(500),
        move || {
            let Some(widget) = weak.upgrade() else { return };
            if !widget.window().is_visible() || widget.get_locked() {
                return;
            }
            let Some(position) = desktop_widget_position(&widget) else {
                return;
            };
            let Ok(mut previous) = last_position.lock() else {
                return;
            };
            if previous.as_ref() == Some(&position) {
                return;
            }
            *previous = Some(position);
            let mut config = load_desktop_widget_config().unwrap_or_default();
            config.x = Some(position.0);
            config.y = Some(position.1);
            config.locked = widget.get_locked();
            if let Err(error) = save_desktop_widget_config(&config) {
                eprintln!("desktop widget position save failed: {error:#}");
                app_log(format!("desktop widget position save failed: {error:#}"));
            }
        },
    );
    timer
}

fn start_desktop_widget_refresh_timer(
    app: &AppWindow,
    widget: &DesktopWidget,
    state: Arc<Mutex<UiState>>,
) -> Timer {
    let weak_app = app.as_weak();
    let weak_widget = widget.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, StdDuration::from_secs(30), move || {
        let (Some(app), Some(widget)) = (weak_app.upgrade(), weak_widget.upgrade()) else {
            return;
        };
        if widget.window().is_visible() {
            let _ = render_all_surfaces(&app, &widget, &state);
        }
    });
    timer
}

#[cfg(target_os = "windows")]
fn begin_desktop_widget_drag(widget: &DesktopWidget) -> Option<DesktopDragOrigin> {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};

    let window = desktop_widget_position(widget)?;
    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) } == 0 {
        app_log("desktop widget drag failed: cursor position unavailable");
        return None;
    }
    app_log(format!(
        "desktop widget custom drag started cursor={},{} window={},{}",
        cursor.x, cursor.y, window.0, window.1
    ));
    Some(DesktopDragOrigin {
        cursor: (cursor.x, cursor.y),
        window,
    })
}

#[cfg(target_os = "macos")]
fn begin_desktop_widget_drag(widget: &DesktopWidget) -> Option<DesktopDragOrigin> {
    use slint::winit_030::WinitWindowAccessor;
    let result = widget
        .window()
        .with_winit_window(|window| window.drag_window());
    if let Some(Err(error)) = result {
        app_log(format!("desktop widget native drag failed: {error}"));
    }
    None
}

#[cfg(target_os = "windows")]
fn continue_desktop_widget_drag(widget: &DesktopWidget, origin: DesktopDragOrigin) {
    use slint::winit_030::{WinitWindowAccessor, winit::dpi::PhysicalPosition};
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};

    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) } == 0 {
        return;
    }
    let x = origin.window.0 + cursor.x - origin.cursor.0;
    let y = origin.window.1 + cursor.y - origin.cursor.1;
    let _ = widget
        .window()
        .with_winit_window(|window| window.set_outer_position(PhysicalPosition::new(x, y)));
}

#[cfg(not(target_os = "windows"))]
fn continue_desktop_widget_drag(_widget: &DesktopWidget, _origin: DesktopDragOrigin) {}

#[cfg(target_os = "windows")]
fn windows_window_handle(
    window: &slint::winit_030::winit::window::Window,
) -> Option<windows_sys::Win32::Foundation::HWND> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as _),
        _ => None,
    }
}

#[cfg(target_os = "windows")]
#[allow(dead_code)]
unsafe extern "system" fn find_desktop_worker_callback(
    top: windows_sys::Win32::Foundation::HWND,
    data: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::core::BOOL {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowExW;

    let shell_view = OsStr::new("SHELLDLL_DefView")
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let worker_class = OsStr::new("WorkerW")
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        let shell = FindWindowExW(
            top,
            std::ptr::null_mut(),
            shell_view.as_ptr(),
            std::ptr::null(),
        );
        if !shell.is_null() {
            let worker = FindWindowExW(
                std::ptr::null_mut(),
                top,
                worker_class.as_ptr(),
                std::ptr::null(),
            );
            if !worker.is_null() {
                *(data as *mut windows_sys::Win32::Foundation::HWND) = worker;
                return 0;
            }
        }
    }
    1
}

#[cfg(target_os = "windows")]
#[allow(dead_code)]
fn windows_desktop_worker() -> Option<windows_sys::Win32::Foundation::HWND> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, FindWindowW, SMTO_NORMAL, SendMessageTimeoutW,
    };

    let progman_class = OsStr::new("Progman")
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        let progman = FindWindowW(progman_class.as_ptr(), std::ptr::null());
        if progman.is_null() {
            return None;
        }
        let mut message_result = 0usize;
        let _ = SendMessageTimeoutW(
            progman,
            0x052c,
            0x0d,
            0,
            SMTO_NORMAL,
            1000,
            &mut message_result,
        );
        let _ = SendMessageTimeoutW(
            progman,
            0x052c,
            0x0d,
            1,
            SMTO_NORMAL,
            1000,
            &mut message_result,
        );
        let mut worker: windows_sys::Win32::Foundation::HWND = std::ptr::null_mut();
        let _ = EnumWindows(
            Some(find_desktop_worker_callback),
            &mut worker as *mut windows_sys::Win32::Foundation::HWND as isize,
        );
        Some(if worker.is_null() { progman } else { worker })
    }
}

#[cfg(target_os = "windows")]
#[allow(dead_code)]
fn attach_windows_widget_to_desktop(window: &slint::winit_030::winit::window::Window) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GWLP_HWNDPARENT, HWND_BOTTOM, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SetWindowLongPtrW, SetWindowPos,
    };

    let (Some(hwnd), Some(worker)) = (windows_window_handle(window), windows_desktop_worker())
    else {
        app_log("desktop widget WorkerW attachment unavailable");
        return;
    };
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, worker as isize);
        let positioned = SetWindowPos(
            hwnd,
            HWND_BOTTOM,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        app_log(format!(
            "desktop widget attached to WorkerW hwnd={hwnd:p} worker={worker:p} positioned={}",
            positioned != 0
        ));
    }
}

#[cfg(target_os = "windows")]
fn apply_windows_widget_shape(window: &slint::winit_030::winit::window::Window) {
    use windows_sys::Win32::{
        Foundation::RECT,
        Graphics::{
            Dwm::{
                DWMNCRP_DISABLED, DWMWA_BORDER_COLOR, DWMWA_NCRENDERING_POLICY,
                DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
            },
            Gdi::{CreateRoundRectRgn, DeleteObject, GetWindowRgnBox, RGN_ERROR, SetWindowRgn},
        },
        UI::WindowsAndMessaging::{
            GWL_EXSTYLE, GWL_STYLE, GetWindowLongW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
            SWP_NOSIZE, SWP_NOZORDER, SetWindowLongW, SetWindowPos, WS_BORDER, WS_CHILD,
            WS_DLGFRAME, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_EX_NOACTIVATE, WS_EX_STATICEDGE,
            WS_EX_TOOLWINDOW, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP,
            WS_SYSMENU, WS_THICKFRAME,
        },
    };

    let Some(hwnd) = windows_window_handle(window) else {
        app_log("desktop widget native shape failed: HWND unavailable");
        return;
    };

    unsafe {
        let style_before = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let frame_style = WS_CHILD
            | WS_THICKFRAME
            | WS_BORDER
            | WS_DLGFRAME
            | WS_MINIMIZEBOX
            | WS_MAXIMIZEBOX
            | WS_SYSMENU;
        let style_after = (style_before & !frame_style) | WS_POPUP;
        let ex_style_before = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let frame_ex_style =
            WS_EX_DLGMODALFRAME | WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE;
        let ex_style_after =
            (ex_style_before & !frame_ex_style) | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
        if style_before != style_after {
            SetWindowLongW(hwnd, GWL_STYLE, style_after as i32);
        }
        if ex_style_before != ex_style_after {
            SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style_after as i32);
        }
        if style_before != style_after || ex_style_before != ex_style_after {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }

        let nc_policy = DWMNCRP_DISABLED;
        let nc_result = DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY as u32,
            &nc_policy as *const i32 as _,
            std::mem::size_of_val(&nc_policy) as u32,
        );
        let corner_preference = DWMWCP_DONOTROUND;
        let corner_result = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &corner_preference as *const i32 as _,
            std::mem::size_of_val(&corner_preference) as u32,
        );
        let border_color = 0xffff_fffeu32;
        let border_result = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            &border_color as *const u32 as _,
            std::mem::size_of_val(&border_color) as u32,
        );

        let size = window.inner_size();
        let corner_diameter = (48.0 * window.scale_factor()).round().max(1.0) as i32;
        let region = CreateRoundRectRgn(
            0,
            0,
            size.width as i32,
            size.height as i32,
            corner_diameter,
            corner_diameter,
        );
        let mut applied = 0;
        if !region.is_null() {
            applied = SetWindowRgn(hwnd, region, 1);
            if applied == 0 {
                DeleteObject(region);
            }
        }
        let mut bounds = RECT::default();
        let region_kind = GetWindowRgnBox(hwnd, &mut bounds);
        app_log(format!(
            "desktop widget native shape hwnd={hwnd:p} style={style_before:#010x}->{style_after:#010x} ex_style={ex_style_before:#010x}->{ex_style_after:#010x} size={}x{} diameter={corner_diameter} region_applied={applied} region_kind={region_kind} nc={nc_result:#010x} corner={corner_result:#010x} border={border_result:#010x}",
            size.width, size.height
        ));
        if region_kind == RGN_ERROR {
            app_log("desktop widget native shape verification failed: no HRGN installed");
        }
    }
}

#[cfg(target_os = "windows")]
fn apply_desktop_widget_window_shape(widget: &DesktopWidget) {
    use slint::winit_030::WinitWindowAccessor;
    let _ = widget
        .window()
        .with_winit_window(apply_windows_widget_shape);
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn begin_desktop_widget_drag(_widget: &DesktopWidget) -> Option<DesktopDragOrigin> {
    None
}

#[cfg(not(target_os = "windows"))]
fn apply_desktop_widget_window_shape(_widget: &DesktopWidget) {}

#[cfg(any(target_os = "windows", target_os = "macos"))]
struct SystemTray {
    _icon: tray_icon::TrayIcon,
    _event_timer: Timer,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn install_system_tray(
    app: &AppWindow,
    widget: &DesktopWidget,
    state: Arc<Mutex<UiState>>,
) -> Result<SystemTray> {
    use tray_icon::{
        MouseButton, TrayIconBuilder, TrayIconEvent,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    };

    let decoded =
        image::load_from_memory(include_bytes!("../assets/schedule-logo-64.png"))?.into_rgba8();
    let (width, height) = decoded.dimensions();
    let icon = tray_icon::Icon::from_rgba(decoded.into_raw(), width, height)
        .map_err(|error| anyhow!("托盘图标无效：{error}"))?;

    let open_item = MenuItem::new("打开主界面", true, None);
    let desktop_item = MenuItem::new("吸附到桌面", true, None);
    let hide_item = MenuItem::new("隐藏到托盘", true, None);
    let separator = PredefinedMenuItem::separator();
    let exit_item = MenuItem::new("同步并退出", true, None);
    let open_id = open_item.id().clone();
    let desktop_id = desktop_item.id().clone();
    let hide_id = hide_item.id().clone();
    let exit_id = exit_item.id().clone();
    let menu = Menu::new();
    menu.append_items(&[
        &open_item,
        &desktop_item,
        &hide_item,
        &separator,
        &exit_item,
    ])?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_menu_on_right_click(true)
        .with_tooltip("Schedule Manager · 双击打开")
        .with_icon(icon)
        .build()?;

    let weak = app.as_weak();
    let weak_widget = widget.as_weak();
    #[cfg(target_os = "macos")]
    let last_left_release = Rc::new(RefCell::new(None::<Instant>));
    let event_timer = Timer::default();
    event_timer.start(
        TimerMode::Repeated,
        StdDuration::from_millis(100),
        move || {
            let Some(app) = weak.upgrade() else { return };
            let Some(widget) = weak_widget.upgrade() else {
                return;
            };
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == open_id {
                    show_main_window(&app, Some(&widget));
                } else if event.id == desktop_id {
                    if let Err(error) = dock_to_desktop(&app, &widget, &state) {
                        app.set_status(format!("吸附到桌面失败：{error}").into());
                    }
                } else if event.id == hide_id {
                    hide_main_window(&app, Some(&widget));
                } else if event.id == exit_id && !app.get_exit_pending() {
                    app.invoke_request_full_exit();
                }
            }
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                match event {
                    TrayIconEvent::DoubleClick {
                        button: MouseButton::Left,
                        ..
                    } => show_main_window(&app, Some(&widget)),
                    #[cfg(target_os = "macos")]
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    } => {
                        let now = Instant::now();
                        let mut previous = last_left_release.borrow_mut();
                        if previous
                            .is_some_and(|value| now.duration_since(value).as_millis() <= 500)
                        {
                            *previous = None;
                            show_main_window(&app, Some(&widget));
                        } else {
                            *previous = Some(now);
                        }
                    }
                    _ => {}
                }
            }
        },
    );

    Ok(SystemTray {
        _icon: tray,
        _event_timer: event_timer,
    })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn install_system_tray(
    _app: &AppWindow,
    _widget: &DesktopWidget,
    _state: Arc<Mutex<UiState>>,
) -> Result<()> {
    Ok(())
}

fn hide_main_window(app: &AppWindow, widget: Option<&DesktopWidget>) {
    app.set_status("已隐藏到系统托盘，双击托盘图标可恢复".into());
    let _ = app.hide();
    #[cfg(target_os = "windows")]
    close_external_desktop_widget();
    if let Some(widget) = widget {
        let _ = widget.hide();
    }
}

fn show_main_window(app: &AppWindow, widget: Option<&DesktopWidget>) {
    #[cfg(target_os = "windows")]
    close_external_desktop_widget();
    if let Some(widget) = widget {
        let _ = widget.hide();
    }
    let _ = app.show();
    app.set_status("主界面已从系统托盘恢复".into());
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        use slint::winit_030::WinitWindowAccessor;
        let _ = app.window().with_winit_window(|window| {
            window.set_minimized(false);
            window.focus_window();
        });
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn begin_system_window_drag(app: &AppWindow) {
    use slint::winit_030::WinitWindowAccessor;
    let _ = app
        .window()
        .with_winit_window(|window| window.drag_window());
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn begin_system_window_drag(_app: &AppWindow) {}

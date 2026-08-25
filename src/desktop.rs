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
use slint::{Color, ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
#[cfg(target_os = "windows")]
use std::process::{Command, Output};
#[cfg(target_os = "macos")]
use std::{cell::RefCell, rc::Rc, time::Instant};
use std::{
    collections::{HashMap, VecDeque},
    ffi::OsStr,
    sync::{Arc, Mutex},
    thread,
    time::Duration as StdDuration,
};
#[cfg(target_os = "macos")]
use std::{fs, path::PathBuf};

slint::include_modules!();

struct UiState {
    visible_month: NaiveDate,
    selected_date: NaiveDate,
    selected_event_id: Option<String>,
    calendar_view: i32,
    token: Option<String>,
    email: Option<String>,
    sync_conflicts: VecDeque<SyncConflict>,
    pending_sync_cursor: Option<i64>,
    sync_in_progress: bool,
}

#[derive(Clone)]
struct SyncConflict {
    local: CalendarEvent,
    remote: Option<CalendarEvent>,
}

pub fn run() -> Result<()> {
    let mut startup_hidden = std::env::args_os().any(|arg| arg == OsStr::new("--startup-hidden"));
    #[cfg(target_os = "windows")]
    let notification_identity_error = crate::windows_notifications::prepare_identity().err();
    let repository = LocalRepository::open()?;
    let today = Local::now().date_naive();
    let email = repository.setting("account_email")?;
    let token = email.as_deref().and_then(load_token);
    let app = AppWindow::new()?;
    let close_behavior = repository
        .setting("close_behavior")?
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|value| matches!(value, 0 | 1 | 2))
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
        pending_sync_cursor: None,
        sync_in_progress: false,
    }));
    render_all(&app, &state)?;
    #[cfg(target_os = "windows")]
    if let Some(error) = notification_identity_error {
        app.set_status(format!("Windows 通知身份注册失败，将使用兼容模式：{error}").into());
    }
    wire_callbacks(&app, state.clone());
    let _system_tray = match install_system_tray(&app) {
        Ok(tray) => Some(tray),
        Err(error) => {
            eprintln!("system tray startup failed: {error:#}");
            startup_hidden = false;
            app.set_close_behavior(-1);
            app.set_status(format!("系统托盘启动失败，请关闭时选择完全退出：{error}").into());
            None
        }
    };
    start_reminder_timer(&app);
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
    if startup_hidden {
        slint::run_event_loop()?;
    } else {
        app.run()?;
    }
    Ok(())
}

fn wire_callbacks(app: &AppWindow, state: Arc<Mutex<UiState>>) {
    let weak = app.as_weak();
    app.on_choose_close_action(move |behavior| {
        let Some(app) = weak.upgrade() else { return };
        if !matches!(behavior, 0 | 1 | 2) {
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
    app.on_request_hide_to_tray(move || {
        if let Some(app) = weak.upgrade() {
            hide_main_window(&app);
        }
    });

    let weak = app.as_weak();
    let shared = state.clone();
    app.on_request_full_exit(move || {
        let Some(app) = weak.upgrade() else { return };
        if app.get_exit_pending() {
            return;
        }
        app.set_exit_pending(true);
        app.set_status("正在同步数据后退出…".into());
        let token = shared.lock().ok().and_then(|state| state.token.clone());
        let weak = app.as_weak();
        thread::spawn(move || {
            let sync_result = token.map(check_sync_consistency).transpose().map(|_| ());
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                if let Err(error) = sync_result {
                    app.set_status(format!("退出前同步失败：{error}").into());
                }
                let _ = mark_watcher_intentional_exit();
                let _ = app.hide();
                let _ = slint::quit_event_loop();
            });
        });
    });

    let weak = app.as_weak();
    app.on_save_settings(move || {
        let Some(app) = weak.upgrade() else { return };
        let close_behavior = app.get_settings_close_behavior().clamp(0, 2);
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
        match event_from_editor(&app, &shared) {
            Ok(event) => {
                let token = shared.lock().ok().and_then(|state| state.token.clone());
                if let Some(token) = token {
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
                            match result {
                                Ok(remote) => {
                                    if let Ok(mut state) = shared_thread.lock() {
                                        state.selected_event_id = Some(remote.id);
                                    }
                                    app.set_editor_visible(false);
                                    app.set_status("日程已保存，云端与本地一致".into());
                                    let _ = render_all(&app, &shared_thread);
                                }
                                Err(error) => app.set_status(
                                    format!("云端保存失败，本地未修改：{error}").into(),
                                ),
                            }
                        });
                    });
                } else {
                    match LocalRepository::open().and_then(|repo| repo.upsert_event(&event)) {
                        Ok(()) => {
                            if let Ok(mut state) = shared.lock() {
                                state.selected_event_id = Some(event.id);
                            }
                            app.set_editor_visible(false);
                            app.set_status("日程已保存到本机，登录后可选择同步".into());
                            let _ = render_all(&app, &shared);
                        }
                        Err(error) => app.set_status(format!("保存失败：{error}").into()),
                    }
                }
            }
            Err(error) => app.set_status(format!("无法保存：{error}").into()),
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
    app.set_event_all_day(event.all_day);
    app.set_event_completed(event.completed);
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
    app.set_event_all_day(false);
    app.set_event_completed(false);
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
        reminder_summary(&event.reminder_minutes)
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

fn start_reminder_timer(app: &AppWindow) {
    deliver_due_reminders(app);
    let timer = Timer::default();
    let weak = app.as_weak();
    timer.start(TimerMode::Repeated, StdDuration::from_secs(30), move || {
        if let Some(app) = weak.upgrade() {
            deliver_due_reminders(&app);
        }
    });
    std::mem::forget(timer);
}

fn deliver_due_reminders(app: &AppWindow) {
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

#[cfg(any(target_os = "windows", target_os = "macos"))]
struct SystemTray {
    _icon: tray_icon::TrayIcon,
    _event_timer: Timer,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn install_system_tray(app: &AppWindow) -> Result<SystemTray> {
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
    let hide_item = MenuItem::new("隐藏到托盘", true, None);
    let separator = PredefinedMenuItem::separator();
    let exit_item = MenuItem::new("同步并退出", true, None);
    let open_id = open_item.id().clone();
    let hide_id = hide_item.id().clone();
    let exit_id = exit_item.id().clone();
    let menu = Menu::new();
    menu.append_items(&[&open_item, &hide_item, &separator, &exit_item])?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_menu_on_right_click(true)
        .with_tooltip("Schedule Manager · 双击打开")
        .with_icon(icon)
        .build()?;

    let weak = app.as_weak();
    #[cfg(target_os = "macos")]
    let last_left_release = Rc::new(RefCell::new(None::<Instant>));
    let event_timer = Timer::default();
    event_timer.start(
        TimerMode::Repeated,
        StdDuration::from_millis(100),
        move || {
            let Some(app) = weak.upgrade() else { return };
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == open_id {
                    show_main_window(&app);
                } else if event.id == hide_id {
                    hide_main_window(&app);
                } else if event.id == exit_id && !app.get_exit_pending() {
                    app.invoke_request_full_exit();
                }
            }
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                match event {
                    TrayIconEvent::DoubleClick {
                        button: MouseButton::Left,
                        ..
                    } => show_main_window(&app),
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
                            show_main_window(&app);
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
fn install_system_tray(_app: &AppWindow) -> Result<()> {
    Ok(())
}

fn hide_main_window(app: &AppWindow) {
    app.set_status("已隐藏到系统托盘，双击托盘图标可恢复".into());
    let _ = app.hide();
}

fn show_main_window(app: &AppWindow) {
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

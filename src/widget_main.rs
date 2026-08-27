#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use chrono::Local;
use std::{
    backtrace::Backtrace,
    fs::{self, OpenOptions},
    io::Write,
};

fn widget_log(message: impl AsRef<str>) {
    let Some(project) = directories::ProjectDirs::from("com", "Emssion", "ScheduleManager") else {
        return;
    };
    let path = project.data_local_dir().join("logs").join(format!(
        "schedule-desktop-widget.{}.log",
        Local::now().format("%Y-%m-%d")
    ));
    let Some(parent) = path.parent() else { return };
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

fn main() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        widget_log(format!(
            "panic: {panic_info}\nbacktrace:\n{}",
            Backtrace::force_capture()
        ));
        original_hook(panic_info);
    }));
    widget_log(format!("starting version={}", env!("CARGO_PKG_VERSION")));
    if let Err(error) = schedule_manager::widget::run() {
        widget_log(format!("stopped with error: {error:#}"));
        eprintln!("Schedule Manager desktop widget stopped: {error:#}");
    } else {
        widget_log("stopped normally");
    }
}

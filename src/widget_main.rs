#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use schedule_manager::widget_diagnostics::{self, log as widget_log};
use std::backtrace::Backtrace;

fn main() {
    // The dump writer must run in another process, before the widget's mutex
    // or UI is initialized, so a stuck renderer cannot block diagnostics.
    #[cfg(target_os = "windows")]
    if let Some(result) = widget_diagnostics::handle_dump_command() {
        if let Err(error) = result {
            widget_log(format!("dump-helper failed: {error:#}"));
            std::process::exit(1);
        }
        return;
    }
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
        std::process::exit(1);
    } else {
        widget_log("stopped normally");
    }
}

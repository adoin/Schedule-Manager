#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    if let Err(error) = schedule_manager::desktop::run() {
        eprintln!("Schedule Manager stopped: {error:#}");
    }
}

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    #[cfg(target_os = "windows")]
    if let Some(result) = schedule_manager::windows_lifecycle::handle_upgrade_command() {
        if let Err(error) = result {
            eprintln!("Upgrade shutdown failed: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = schedule_manager::desktop::run() {
        eprintln!("Schedule Manager stopped: {error:#}");
    }
}

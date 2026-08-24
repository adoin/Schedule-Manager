#[cfg(target_os = "windows")]
fn main() {
    let result = notify_rust::Notification::new()
        .summary("Schedule Manager 通知诊断")
        .body("如果你看到这条消息，说明 Windows Toast 通道可以正常展示。")
        .app_id(schedule_manager::windows_notifications::APP_USER_MODEL_ID)
        .show();

    match result {
        Ok(_) => {
            schedule_manager::windows_notifications::play_reminder_sound();
            println!("notification probe: Windows API accepted the Schedule Manager toast");
        }
        Err(error) => {
            eprintln!("notification probe failed: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    let result = notify_rust::Notification::new()
        .summary("Schedule Manager 通知诊断")
        .body("如果你看到这条消息，说明 macOS 通知通道可以正常展示。")
        .sound_name("default")
        .show();
    match result {
        Ok(_) => println!("notification probe: macOS accepted the Schedule Manager notification"),
        Err(error) => {
            eprintln!("notification probe failed: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn main() {
    eprintln!("notification probe is supported on Windows and macOS only");
}

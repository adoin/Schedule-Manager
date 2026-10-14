#[cfg(target_os = "windows")]
fn main() {
    if let Some(url) = std::env::args().nth(1) {
        match schedule_manager::windows_notifications::show_link_notification(
            "Schedule Manager 链接通知诊断",
            "点击通知正文或“打开链接”按钮，应在默认浏览器打开指定网页。",
            &url,
            true,
        ) {
            Ok(()) => println!("notification probe: Windows accepted the protocol link toast"),
            Err(error) => {
                eprintln!("notification link probe failed: {error:#}");
                std::process::exit(1);
            }
        }
        return;
    }
    if let Err(error) = schedule_manager::windows_notifications::prepare_identity() {
        eprintln!("notification identity preparation failed: {error:#}");
        std::process::exit(1);
    }
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

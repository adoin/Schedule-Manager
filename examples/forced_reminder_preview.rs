//! Render the actual reminder component without adding reminders to the user's database.
//! cargo run --example forced_reminder_preview -- target/forced-reminder-link.png
//! Append --no-link or --error to inspect the other states.
#[cfg(all(feature = "desktop", any(target_os = "windows", target_os = "macos")))]
fn main() -> anyhow::Result<()> {
    use schedule_manager::desktop::ForcedReminderWindow;
    use slint::ComponentHandle;
    let args: Vec<String> = std::env::args().collect();
    let output = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "target/forced-reminder-link.png".into());
    let reminder = ForcedReminderWindow::new()?;
    reminder.set_reminder_title("每日点饭".into());
    reminder.set_reminder_body("今天 11:30 开始".into());
    if !args.iter().any(|arg| arg == "--no-link") {
        reminder.set_reminder_url("https://example.com/lunch".into());
    }
    if args.iter().any(|arg| arg == "--error") {
        reminder.set_link_error("打开链接失败：无法启动默认浏览器".into());
    }
    let weak = reminder.as_weak();
    let mut captured = false;
    reminder.window().set_rendering_notifier(move |state, _| {
        if !matches!(state, slint::RenderingState::AfterRendering) || captured {
            return;
        }
        captured = true;
        let result = (|| -> anyhow::Result<()> {
            let reminder = weak.upgrade().unwrap();
            let pixels = reminder.window().take_snapshot()?;
            image::save_buffer(
                &output,
                pixels.as_bytes(),
                pixels.width(),
                pixels.height(),
                image::ColorType::Rgba8,
            )?;
            println!("Saved {output}");
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("Preview failed: {error:#}");
            std::process::exit(1);
        }
        slint::quit_event_loop().unwrap();
    })?;
    slint::Timer::single_shot(std::time::Duration::from_secs(10), || {
        eprintln!("Preview failed: no frame rendered within 10 seconds");
        std::process::exit(1);
    });
    reminder.run()?;
    Ok(())
}

#[cfg(not(all(feature = "desktop", any(target_os = "windows", target_os = "macos"))))]
fn main() {}

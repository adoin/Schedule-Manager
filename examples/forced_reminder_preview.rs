//! Render the actual reminder component without adding reminders to the user's database.
//! cargo run --example forced_reminder_preview -- target/forced-reminder-link.png
//! Append --no-link or --error to inspect the other states.
//! --event-id ID reads a real saved event; --open-link clicks its actual link button.
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
    if let Some(index) = args.iter().position(|arg| arg == "--event-id") {
        let id = args
            .get(index + 1)
            .ok_or_else(|| anyhow::anyhow!("Missing event ID"))?;
        let event = schedule_manager::repository::LocalRepository::open()?
            .event(id)?
            .ok_or_else(|| anyhow::anyhow!("Event not found"))?;
        reminder.set_reminder_title(event.title.into());
        reminder.set_reminder_url(event.notification_url.into());
    }
    schedule_manager::desktop::wire_reminder_link_handler(&reminder);
    let open_link = args.iter().any(|arg| arg == "--open-link");
    if open_link {
        anyhow::ensure!(
            !reminder.get_reminder_url().is_empty(),
            "Saved event has no notification link"
        );
    }
    if args.iter().any(|arg| arg == "--error") {
        reminder.set_link_error("打开链接失败：无法启动默认浏览器".into());
    }
    let weak = reminder.as_weak();
    let mut captured = false;
    let mut clicked = false;
    reminder.window().set_rendering_notifier(move |state, _| {
        if !matches!(state, slint::RenderingState::AfterRendering) || captured {
            return;
        }
        if open_link && !clicked {
            clicked = true;
            let weak = weak.clone();
            slint::invoke_from_event_loop(move || {
                let reminder = weak.upgrade().unwrap();
                reminder.set_link_error("Link click has not been verified".into());
                let position = slint::LogicalPosition::new(110.0, 190.0);
                let button = slint::platform::PointerEventButton::Left;
                reminder
                    .window()
                    .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                        position,
                        button,
                    });
                reminder
                    .window()
                    .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                        position,
                        button,
                    });
                reminder.window().request_redraw();
            })
            .unwrap();
            return;
        }
        captured = true;
        let result = (|| -> anyhow::Result<()> {
            let reminder = weak.upgrade().unwrap();
            if open_link {
                anyhow::ensure!(
                    reminder.get_link_error().is_empty(),
                    "Link click failed: {}",
                    reminder.get_link_error()
                );
            }
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

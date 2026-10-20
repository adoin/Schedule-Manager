//! Render account-dialog states without accessing user data or credentials.
//! Pass an output PNG, then optional --signed-in, --register, --error, --long-error, --small, --scroll-bottom.
#[cfg(all(feature = "desktop", any(target_os = "windows", target_os = "macos")))]
fn main() -> anyhow::Result<()> {
    use schedule_manager::desktop::AppWindow;
    use slint::ComponentHandle;
    let args: Vec<String> = std::env::args().collect();
    let output = args
        .get(1)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Pass an output PNG path"))?;
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    let app = AppWindow::new()?;
    app.set_auth_visible(true);
    app.set_signed_in(has("--signed-in"));
    app.set_register_mode(has("--register"));
    app.set_auth_email("name@example.com".into());
    app.set_editor_visible(true);
    app.set_event_title("待保存的日程".into());
    if has("--error") {
        app.set_auth_message("登录已过期，请重新登录；登录后请再次点击保存".into());
    }
    if has("--long-error") {
        app.set_auth_message("连接服务失败，请检查网络后重试。".repeat(35).into());
    }
    if has("--small") {
        app.window()
            .set_size(slint::LogicalSize::new(1040.0, 680.0));
    }
    let weak = app.as_weak();
    let mut captured = false;
    let scroll_bottom = has("--scroll-bottom");
    let mut scrolled = false;
    app.window().set_rendering_notifier(move |state, _| {
        if !matches!(state, slint::RenderingState::AfterRendering) || captured {
            return;
        }
        if scroll_bottom && !scrolled {
            scrolled = true;
            let weak = weak.clone();
            slint::invoke_from_event_loop(move || {
                let app = weak.upgrade().unwrap();
                app.window()
                    .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                        position: slint::LogicalPosition::new(520.0, 500.0),
                        delta_x: 0.0,
                        delta_y: -2000.0,
                    });
                app.window().request_redraw();
            })
            .unwrap();
            return;
        }
        captured = true;
        let result = (|| -> anyhow::Result<()> {
            let app = weak.upgrade().unwrap();
            let pixels = app.window().take_snapshot()?;
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
    app.run()?;
    Ok(())
}

#[cfg(not(all(feature = "desktop", any(target_os = "windows", target_os = "macos"))))]
fn main() {}

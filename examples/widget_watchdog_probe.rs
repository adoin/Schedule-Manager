//! Exercise hang capture and shutdown without creating or suspending any UI.
//! cargo run --example widget_watchdog_probe -- stall
//! cargo run --example widget_watchdog_probe -- exit
#[cfg(all(feature = "desktop", target_os = "windows"))]
fn main() -> anyhow::Result<()> {
    use schedule_manager::widget_diagnostics as diagnostics;
    if let Some(result) = diagnostics::handle_dump_command() {
        return result;
    }
    let mode = std::env::args().nth(1).unwrap_or_default();
    anyhow::ensure!(mode == "stall" || mode == "exit", "expected stall or exit");
    diagnostics::log(format!("watchdog-probe mode={mode}"));
    diagnostics::start();
    if mode == "exit" {
        diagnostics::request_exit();
        // The watchdog must terminate this process within its capture budget.
        std::thread::sleep(std::time::Duration::from_secs(25));
        anyhow::bail!("watchdog failed to terminate probe");
    }
    std::thread::sleep(std::time::Duration::from_secs(20));
    diagnostics::progress(1);
    std::thread::sleep(std::time::Duration::from_secs(2));
    Ok(())
}

#[cfg(not(all(feature = "desktop", target_os = "windows")))]
fn main() {}

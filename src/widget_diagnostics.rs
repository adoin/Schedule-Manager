//! Low-overhead UI progress markers and an independent Windows hang recorder.
use chrono::Local;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicUsize, Ordering},
};

static PROGRESS: AtomicU64 = AtomicU64::new(0);
static PHASE: AtomicUsize = AtomicUsize::new(0);
static WINDOW: AtomicIsize = AtomicIsize::new(0);
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);
const PHASES: &[&str] = &[
    "startup",
    "logic",
    "native-maintain",
    "data-refresh",
    "ui",
    "render-or-event-loop",
];

pub fn data_dir() -> Option<PathBuf> {
    Some(
        directories::ProjectDirs::from("com", "Emssion", "ScheduleManager")?
            .data_local_dir()
            .into(),
    )
}

pub fn log(message: impl AsRef<str>) {
    let Some(directory) = data_dir().map(|path| path.join("logs")) else {
        return;
    };
    if fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join(format!(
        "schedule-desktop-widget.{}.log",
        Local::now().format("%Y-%m-%d")
    ));
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        // No shared log mutex: the watchdog must still log if the UI is stuck.
        let _ = file.write_all(
            format!(
                "{} pid={} {}\n",
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                std::process::id(),
                message.as_ref()
            )
            .as_bytes(),
        );
    }
}

pub fn progress(phase: usize) {
    PHASE.store(phase, Ordering::Relaxed);
    PROGRESS.fetch_add(1, Ordering::Release);
}

pub fn set_window(window: isize) {
    WINDOW.store(window, Ordering::Relaxed);
}

pub fn request_exit() {
    if !EXIT_REQUESTED.swap(true, Ordering::AcqRel) {
        log("exit-requested; waiting for event loop and renderer teardown");
    }
}

pub fn exit_requested() -> bool {
    EXIT_REQUESTED.load(Ordering::Acquire)
}

// Returns true once per stall, then rearms only after progress resumes.
#[derive(Default)]
struct StallTracker {
    progress: u64,
    idle_seconds: u64,
    reported: bool,
}

impl StallTracker {
    fn tick(&mut self, progress: u64, gap_seconds: u64) -> bool {
        if progress != self.progress || gap_seconds > 5 {
            self.progress = progress;
            self.idle_seconds = 0;
            self.reported = false;
        } else {
            self.idle_seconds += gap_seconds;
        }
        if self.idle_seconds >= 15 && !self.reported {
            self.reported = true;
            return true;
        }
        false
    }
}

#[cfg(target_os = "windows")]
pub fn start() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    thread::spawn(|| {
        let mut tracker = StallTracker::default();
        let mut last_tick = Instant::now();
        let mut last_summary = Instant::now();
        let mut exit_at = None;
        let mut dump_count = 0;
        log(format!(
            "watchdog-started renderer=glow arch={} exe={:?}",
            std::env::consts::ARCH,
            std::env::current_exe()
        ));
        loop {
            thread::sleep(Duration::from_secs(1));
            let gap = last_tick.elapsed().as_secs();
            last_tick = Instant::now();
            let was_stalled = tracker.reported;
            let stalled = tracker.tick(PROGRESS.load(Ordering::Acquire), gap);
            if was_stalled && !tracker.reported {
                log(format!("watchdog-progress-resumed {}", snapshot()));
            }
            let manual = data_dir()
                .is_some_and(|dir| fs::remove_file(dir.join("widget-diagnostic.signal")).is_ok());
            if data_dir().is_some_and(|dir| dir.join("desktop-widget-exit.signal").exists()) {
                request_exit();
            }
            if exit_requested() && exit_at.is_none() {
                exit_at = Some(Instant::now());
                log(format!("watchdog-exit-observed {}", snapshot()));
            }
            let exit_timeout = exit_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(8));
            if stalled || manual || exit_timeout {
                let reason = if exit_timeout {
                    "exit-timeout"
                } else if manual {
                    "manual"
                } else {
                    "ui-stall"
                };
                log(format!(
                    "diagnostic-trigger reason={reason} idle_seconds={} {}",
                    tracker.idle_seconds,
                    snapshot()
                ));
                if dump_count < 3 {
                    dump_count += 1;
                    if let Err(error) = capture_dump(reason) {
                        log(format!("dump-failed: {error:#}"));
                    }
                } else {
                    log("dump-skipped session-limit=3");
                }
                // Time spent writing the dump is not a sleep/resume event.
                last_tick = Instant::now();
                if exit_timeout {
                    // Only after an explicit close request. This read-only widget
                    // owns no sync work; the manager continues its final sync.
                    log("exit-timeout; terminating widget after diagnostic capture");
                    std::process::exit(0);
                }
            }
            if last_summary.elapsed() >= Duration::from_secs(60) {
                log(format!("watchdog-heartbeat {}", snapshot()));
                last_summary = Instant::now();
            }
        }
    });
}

#[cfg(target_os = "windows")]
fn snapshot() -> String {
    use windows_sys::Win32::{
        Foundation::{HWND, RECT},
        UI::WindowsAndMessaging::{GetParent, GetWindowRect, IsWindow, IsWindowVisible},
    };
    let window = WINDOW.load(Ordering::Relaxed) as HWND;
    let mut rect = RECT::default();
    unsafe {
        let parent = GetParent(window);
        let rect_ok = GetWindowRect(window, &mut rect);
        format!(
            "phase={} progress={} hwnd={window:p} valid={} visible={} parent={parent:p} parent_valid={} rect_ok={rect_ok} rect={},{},{},{}",
            PHASES
                .get(PHASE.load(Ordering::Relaxed))
                .unwrap_or(&"unknown"),
            PROGRESS.load(Ordering::Acquire),
            IsWindow(window),
            IsWindowVisible(window),
            IsWindow(parent),
            rect.left,
            rect.top,
            rect.right,
            rect.bottom
        )
    }
}

#[cfg(target_os = "windows")]
fn capture_dump(reason: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::{
        os::windows::process::CommandExt,
        process::Command,
        thread,
        time::{Duration, Instant},
    };
    let directory = data_dir()
        .context("data directory unavailable")?
        .join("logs/dumps");
    fs::create_dir_all(&directory)?;
    // Bound accumulation across sessions as well as within this run.
    let mut dumps: Vec<_> = fs::read_dir(&directory)?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with("widget-")
                && entry.path().extension().is_some_and(|ext| ext == "dmp")
        })
        .collect();
    dumps.sort_by_key(|entry| entry.file_name());
    let remove_count = dumps.len().saturating_sub(9);
    for entry in dumps.into_iter().take(remove_count) {
        fs::remove_file(entry.path())?;
    }
    let path = directory.join(format!(
        "widget-{}-{}-{reason}.dmp",
        Local::now().format("%Y%m%d-%H%M%S-%3f"),
        std::process::id()
    ));
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--capture-dump", &std::process::id().to_string()])
        .arg(&path)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .spawn()?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::ensure!(status.success(), "dump helper exited {status}");
            log(format!("dump-saved path={}", path.display()));
            return Ok(());
        }
        if started.elapsed() >= Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&path);
            anyhow::bail!("dump helper exceeded 10 seconds");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(target_os = "windows")]
pub fn handle_dump_command() -> Option<anyhow::Result<()>> {
    let mut args = std::env::args_os().skip(1);
    if args.next()?.to_str()? != "--capture-dump" {
        return None;
    }
    Some((|| {
        use anyhow::Context;
        use std::{fs::File, os::windows::io::AsRawHandle};
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::{
                Diagnostics::Debug::{
                    MiniDumpWithThreadInfo, MiniDumpWithUnloadedModules, MiniDumpWriteDump,
                },
                Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ},
            },
        };
        let pid: u32 = args
            .next()
            .context("missing target PID")?
            .to_str()
            .context("invalid PID")?
            .parse()?;
        let path = PathBuf::from(args.next().context("missing dump path")?);
        let file = File::create(&path)?;
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
            anyhow::ensure!(
                !process.is_null(),
                "OpenProcess: {}",
                std::io::Error::last_os_error()
            );
            let ok = MiniDumpWriteDump(
                process,
                pid,
                file.as_raw_handle(),
                MiniDumpWithThreadInfo | MiniDumpWithUnloadedModules,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            );
            let error = std::io::Error::last_os_error();
            CloseHandle(process);
            if ok == 0 {
                drop(file);
                let _ = fs::remove_file(path);
                anyhow::bail!("MiniDumpWriteDump: {error}");
            }
        }
        Ok(())
    })())
}

#[cfg(test)]
mod tests {
    use super::StallTracker;

    #[test]
    fn reports_once_and_rearms_after_recovery() {
        let mut tracker = StallTracker::default();
        for _ in 0..14 {
            assert!(!tracker.tick(0, 1));
        }
        assert!(tracker.tick(0, 1));
        assert!(!tracker.tick(0, 1));
        assert!(!tracker.tick(1, 1));
        for _ in 0..14 {
            assert!(!tracker.tick(1, 1));
        }
        assert!(tracker.tick(1, 1));
    }

    #[test]
    fn suspend_or_scheduler_gap_does_not_count_as_a_ui_stall() {
        let mut tracker = StallTracker::default();
        for _ in 0..14 {
            assert!(!tracker.tick(0, 1));
        }
        assert!(!tracker.tick(0, 3600));
        assert!(!tracker.tick(0, 1));
    }
}

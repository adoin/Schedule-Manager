//! Bounded upgrade shutdown scoped to one installation, plus widget ownership.
use anyhow::{Context, Result, anyhow};
use std::{
    ffi::OsStr,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_INVALID_PARAMETER, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Threading::{
            CreateEventW, GetCurrentProcessId, INFINITE, OpenProcess,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
            QueryFullProcessImageNameW, SetEvent, TerminateProcess, WaitForSingleObject,
        },
    },
};

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn owned(handle: HANDLE) -> Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn normalized(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .trim_end_matches(['\\', '/'])
        .replace('/', "\\")
        .to_lowercase()
}

fn event_for(directory: &Path) -> Result<OwnedHandle> {
    let directory = std::fs::canonicalize(directory)?;
    // Stable, directory-scoped name; other installed/portable copies are isolated.
    let hash = normalized(&directory)
        .bytes()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    let name = wide(OsStr::new(&format!(
        r"Local\Emssion.ScheduleManager.Upgrade.{hash:016x}"
    )));
    owned(unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) })
}

pub fn upgrade_requested() -> bool {
    static SIGNAL: OnceLock<Option<OwnedHandle>> = OnceLock::new();
    SIGNAL
        .get_or_init(|| {
            let exe = std::env::current_exe().ok()?;
            event_for(exe.parent()?).ok()
        })
        .as_ref()
        .is_some_and(|event| unsafe {
            WaitForSingleObject(event.as_raw_handle(), 0) == WAIT_OBJECT_0
        })
}

fn installed_binary(path: &Path, directory: &Path) -> bool {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path = resolved.as_path();
    let known = path.file_name().is_some_and(|name| {
        [
            "ScheduleManager.exe",
            "schedule-manager.exe",
            "schedule-desktop-widget.exe",
        ]
        .iter()
        .any(|candidate| name.to_string_lossy().eq_ignore_ascii_case(candidate))
    });
    known
        && path
            .parent()
            .is_some_and(|parent| normalized(parent) == normalized(directory))
}

fn installation_processes(directory: &Path) -> Result<Vec<OwnedHandle>> {
    let snapshot = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })?;
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut processes = Vec::new();
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) } != 0;
    while more {
        let name = String::from_utf16_lossy(
            &entry.szExeFile[..entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len())],
        );
        if entry.th32ProcessID != unsafe { GetCurrentProcessId() }
            && [
                "ScheduleManager.exe",
                "schedule-manager.exe",
                "schedule-desktop-widget.exe",
            ]
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
        {
            let handle = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                    0,
                    entry.th32ProcessID,
                )
            };
            if handle.is_null() {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_INVALID_PARAMETER as i32) {
                    return Err(error).context("无法检查日程程序进程，请手动完全退出后重试");
                }
            } else {
                let handle = owned(handle)?;
                let mut path = vec![0u16; 32768];
                let mut length = path.len() as u32;
                if unsafe {
                    QueryFullProcessImageNameW(
                        handle.as_raw_handle(),
                        0,
                        path.as_mut_ptr(),
                        &mut length,
                    )
                } == 0
                {
                    if unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } != WAIT_OBJECT_0 {
                        return Err(std::io::Error::last_os_error())
                            .context("无法确认运行程序的安装目录");
                    }
                } else if installed_binary(
                    &PathBuf::from(String::from_utf16_lossy(&path[..length as usize])),
                    directory,
                ) {
                    processes.push(handle);
                }
            }
        }
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    Ok(processes)
}

fn exited(process: &OwnedHandle) -> bool {
    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_OBJECT_0 }
}

pub fn stop_installation(directory: &Path) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    let directory = std::fs::canonicalize(directory).context("无法确认安装目录")?;
    anyhow::ensure!(directory.is_dir(), "安装目录无效");
    let signal = event_for(&directory)?;
    anyhow::ensure!(
        unsafe { SetEvent(signal.as_raw_handle()) } != 0,
        "无法发送升级退出请求"
    );
    // Hold process handles throughout shutdown, so a reused PID cannot be killed.
    let mut processes = installation_processes(&directory)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while processes.iter().any(|process| !exited(process)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    // Include children started while an older main process was shutting down.
    processes.extend(installation_processes(&directory)?);
    let mut termination_error = None;
    for process in &processes {
        if !exited(process) && unsafe { TerminateProcess(process.as_raw_handle(), 0) } == 0 {
            // A duplicate handle or a graceful-exit race can report ACCESS_DENIED
            // while teardown is underway. Judge the final process state below.
            termination_error = Some(std::io::Error::last_os_error());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while processes.iter().any(|process| !exited(process)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if !processes.iter().all(exited) {
        if let Some(error) = termination_error {
            return Err(error).context("无法关闭旧版日程程序，请手动完全退出后重试");
        }
        return Err(anyhow!("旧进程仍占用安装文件，请完全退出后重试"));
    }
    anyhow::ensure!(
        installation_processes(&directory)?.iter().all(exited),
        "旧进程重新启动，请完全退出后重试"
    );
    Ok(())
}

pub fn handle_upgrade_command() -> Option<Result<()>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args
        .get(1)
        .is_none_or(|arg| arg != "--shutdown-for-upgrade")
    {
        return None;
    }
    Some(if args.len() == 3 {
        stop_installation(Path::new(&args[2]))
    } else {
        Err(anyhow!("升级退出命令需要一个安装目录"))
    })
}

pub fn watch_parent() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    let Some(index) = args.iter().position(|arg| arg == "--parent-pid") else {
        return Ok(());
    };
    let pid: u32 = args
        .get(index + 1)
        .and_then(|value| value.to_str())
        .context("缺少主程序进程编号")?
        .parse()?;
    watch_parent_id(pid)
}

fn watch_parent_id(pid: u32) -> Result<()> {
    anyhow::ensure!(pid != 0, "主程序进程编号无效");
    let raw = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if raw.is_null()
        && std::io::Error::last_os_error().raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32)
    {
        std::process::exit(0);
    }
    let parent = owned(raw).context("无法监测主程序生命周期")?;
    let mut image = vec![0u16; 32768];
    let mut length = image.len() as u32;
    if unsafe {
        QueryFullProcessImageNameW(parent.as_raw_handle(), 0, image.as_mut_ptr(), &mut length)
    } == 0
    {
        if exited(&parent) {
            std::process::exit(0);
        }
        return Err(std::io::Error::last_os_error()).context("无法确认主程序身份");
    }
    let image = PathBuf::from(String::from_utf16_lossy(&image[..length as usize]));
    let executable = std::env::current_exe()?;
    if !image.file_name().is_some_and(|name| {
        ["ScheduleManager.exe", "schedule-manager.exe"]
            .iter()
            .any(|expected| name.to_string_lossy().eq_ignore_ascii_case(expected))
    }) || !installed_binary(&image, executable.parent().context("挂件目录无效")?)
    {
        // The parent may have exited before startup and its PID been reused.
        std::process::exit(0);
    }
    std::thread::spawn(move || {
        if unsafe { WaitForSingleObject(parent.as_raw_handle(), INFINITE) } == WAIT_OBJECT_0 {
            #[cfg(not(test))]
            {
                crate::widget_diagnostics::log("parent-exited; closing owned desktop widget");
                crate::widget_diagnostics::request_exit();
            }
            // Test fixtures emulate an unresponsive renderer and do not write
            // to the user's runtime log directory.
            // Independent of the renderer: this widget has no cloud save work.
            std::thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    #[test]
    fn only_known_executables_in_the_exact_directory_match() {
        let root = Path::new(r"C:\Apps\Schedule Manager");
        assert!(installed_binary(
            Path::new(r"c:\apps\SCHEDULE MANAGER\ScheduleManager.exe"),
            root
        ));
        assert!(installed_binary(
            Path::new(r"C:\Apps\Schedule Manager\schedule-desktop-widget.exe"),
            root
        ));
        assert!(!installed_binary(
            Path::new(r"C:\Apps\Schedule Manager Backup\ScheduleManager.exe"),
            root
        ));
        assert!(!installed_binary(
            Path::new(r"C:\Apps\Schedule Manager\other.exe"),
            root
        ));
    }

    fn root() -> PathBuf {
        let base = std::env::var_os("SCHEDULE_TEST_ARTIFACT_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("RUNNER_TEMP").map(PathBuf::from))
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-artifacts")
            });
        let root = base.join(format!("lifecycle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    struct Fixture {
        child: Child,
        path: PathBuf,
    }
    impl Fixture {
        fn spawn(
            root: &Path,
            directory: &str,
            name: &str,
            cooperative: bool,
            parent: Option<u32>,
        ) -> Self {
            let directory = root.join(directory);
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join(name);
            std::fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
            let mut command = Command::new(&path);
            command
                .args([
                    "--ignored",
                    "--exact",
                    "windows_lifecycle::tests::process_fixture",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .env(
                    "SCHEDULE_FIXTURE_COOPERATIVE",
                    if cooperative { "1" } else { "0" },
                );
            if let Some(parent) = parent {
                command.env("SCHEDULE_FIXTURE_PARENT", parent.to_string());
            }
            let child = command.spawn().unwrap();
            Self { child, path }
        }
        fn wait_bounded(&mut self) -> Option<std::process::ExitStatus> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return Some(status);
                }
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(self.path.with_extension("graceful"));
            let _ = std::fs::remove_dir(self.path.parent().unwrap());
        }
    }

    #[test]
    #[ignore = "child process fixture, invoked explicitly by lifecycle tests"]
    fn process_fixture() {
        if let Ok(pid) = std::env::var("SCHEDULE_FIXTURE_PARENT") {
            watch_parent_id(pid.parse().unwrap()).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if std::env::var("SCHEDULE_FIXTURE_COOPERATIVE").as_deref() == Ok("1")
                && upgrade_requested()
            {
                std::fs::write(
                    std::env::current_exe().unwrap().with_extension("graceful"),
                    b"exited",
                )
                .unwrap();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn upgrade_stops_legacy_and_cooperative_processes_without_touching_other_copy() {
        let root = root();
        let mut legacy = Fixture::spawn(&root, "upgrade", "ScheduleManager.exe", false, None);
        let mut widget =
            Fixture::spawn(&root, "upgrade", "schedule-desktop-widget.exe", true, None);
        let mut other = Fixture::spawn(&root, "other-copy", "ScheduleManager.exe", false, None);
        let started = Instant::now();
        stop_installation(&root.join("upgrade")).unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(legacy.wait_bounded().unwrap().success());
        assert!(widget.wait_bounded().unwrap().success());
        assert!(widget.path.with_extension("graceful").is_file());
        assert!(other.child.try_wait().unwrap().is_none());
        drop((legacy, widget, other));
        let _ = std::fs::remove_dir(root);
    }

    #[test]
    fn widget_exits_even_when_parent_is_force_killed() {
        let root = root();
        let mut parent = Fixture::spawn(&root, "owned-app", "ScheduleManager.exe", false, None);
        let mut widget = Fixture::spawn(
            &root,
            "owned-app",
            "schedule-desktop-widget.exe",
            false,
            Some(parent.child.id()),
        );
        std::thread::sleep(Duration::from_millis(300));
        parent.child.kill().unwrap();
        parent.child.wait().unwrap();
        assert!(widget.wait_bounded().unwrap().success());
        drop((parent, widget));
        let _ = std::fs::remove_dir(root);
    }
}

use anyhow::{Context, Result};
use std::{os::windows::ffi::OsStrExt, path::PathBuf};
use windows::{
    Win32::{
        Storage::EnhancedStorage::PKEY_AppUserModel_ID,
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            IPersistFile, StructuredStorage::PROPVARIANT,
        },
        System::Diagnostics::Debug::MessageBeep,
        UI::Shell::{
            IShellLinkW, PropertiesSystem::IPropertyStore, SetCurrentProcessExplicitAppUserModelID,
            ShellLink,
        },
        UI::WindowsAndMessaging::MB_ICONEXCLAMATION,
    },
    core::{Interface, PCWSTR},
};

pub const APP_USER_MODEL_ID: &str = "Emssion.ScheduleManager";

pub fn prepare_identity() -> Result<()> {
    let app_id = wide(APP_USER_MODEL_ID);
    let executable = std::env::current_exe().context("无法确定程序路径")?;
    let executable_wide = wide_os(executable.as_os_str());
    let working_directory = executable
        .parent()
        .context("程序路径没有父目录")?
        .to_path_buf();
    let working_directory_wide = wide_os(working_directory.as_os_str());
    let shortcut = shortcut_path()?;
    if let Some(parent) = shortcut.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let shortcut_wide = wide_os(shortcut.as_os_str());

    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .context("初始化 Windows COM 失败")?;
        SetCurrentProcessExplicitAppUserModelID(PCWSTR(app_id.as_ptr()))
            .context("设置进程 AppUserModelID 失败")?;

        let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .context("创建开始菜单快捷方式失败")?;
        shell_link
            .SetPath(PCWSTR(executable_wide.as_ptr()))
            .context("设置快捷方式目标失败")?;
        shell_link
            .SetWorkingDirectory(PCWSTR(working_directory_wide.as_ptr()))
            .context("设置快捷方式工作目录失败")?;
        shell_link
            .SetIconLocation(PCWSTR(executable_wide.as_ptr()), 0)
            .context("设置快捷方式图标失败")?;

        let property_store: IPropertyStore =
            shell_link.cast().context("读取快捷方式属性存储失败")?;
        let app_id_value = PROPVARIANT::from(APP_USER_MODEL_ID);
        property_store
            .SetValue(&PKEY_AppUserModel_ID, &app_id_value)
            .context("写入快捷方式 AppUserModelID 失败")?;
        property_store.Commit().context("提交快捷方式属性失败")?;

        let persist_file: IPersistFile = shell_link.cast().context("保存快捷方式失败")?;
        persist_file
            .Save(PCWSTR(shortcut_wide.as_ptr()), true)
            .context("写入开始菜单快捷方式失败")?;
    }
    Ok(())
}

pub fn play_reminder_sound() {
    unsafe {
        let _ = MessageBeep(MB_ICONEXCLAMATION);
    }
}

fn shortcut_path() -> Result<PathBuf> {
    let app_data = std::env::var_os("APPDATA").context("APPDATA 环境变量不存在")?;
    Ok(PathBuf::from(app_data)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Schedule Manager.lnk"))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_os(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

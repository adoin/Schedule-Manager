use anyhow::{Context, Result};
use std::{os::windows::ffi::OsStrExt, path::PathBuf};
use windows::{
    Data::Xml::Dom::XmlDocument,
    UI::Notifications::{ToastNotification, ToastNotificationManager},
    Win32::{
        Storage::EnhancedStorage::PKEY_AppUserModel_ID,
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            IPersistFile, StructuredStorage::PROPVARIANT,
        },
        System::Diagnostics::Debug::MessageBeep,
        UI::Shell::{
            IShellLinkW, PropertiesSystem::IPropertyStore, SetCurrentProcessExplicitAppUserModelID,
            ShellExecuteW, ShellLink,
        },
        UI::WindowsAndMessaging::{MB_ICONEXCLAMATION, SW_SHOWNORMAL},
    },
    core::{HSTRING, Interface, PCWSTR},
};

pub const APP_USER_MODEL_ID: &str = "Emssion.ScheduleManager";

/// Protocol activation lets Windows open the link even after this process exits
/// or the toast moves to Action Center. No in-process activation callback is needed.
pub fn show_link_notification(title: &str, body: &str, url: &str, sound: bool) -> Result<()> {
    let url = url::Url::parse(url).context("通知链接格式无效")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "通知链接必须是 http 或 https 网页地址"
    );
    prepare_identity().context("注册链接通知身份失败")?;
    let document = XmlDocument::new()?;
    document.LoadXml(&HSTRING::from(link_notification_xml(
        title,
        body,
        url.as_str(),
    )))?;
    let toast = ToastNotification::CreateToastNotification(&document)?;
    let notifier =
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_USER_MODEL_ID))?;
    notifier.Show(&toast).context("发送链接通知失败")?;
    if sound {
        play_reminder_sound();
    }
    Ok(())
}

fn link_notification_xml(title: &str, body: &str, url: &str) -> String {
    let url = escape_xml(url);
    format!(
        "<toast activationType=\"protocol\" launch=\"{url}\"><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual><actions><action content=\"打开链接\" activationType=\"protocol\" arguments=\"{url}\"/></actions><audio silent=\"true\"/></toast>",
        escape_xml(title),
        escape_xml(body)
    )
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_toast_uses_protocol_for_body_and_button_and_escapes_content() {
        let xml = link_notification_xml(
            "会议 <提醒>",
            "A & B",
            "https://example.com/?a=1&b=\"中文\"",
        );
        assert!(xml.starts_with("<toast activationType=\"protocol\" launch=\"https://example.com/?a=1&amp;b=&quot;中文&quot;\">"));
        assert!(xml.contains("<action content=\"打开链接\" activationType=\"protocol\" arguments=\"https://example.com/?a=1&amp;b=&quot;中文&quot;\"/>"));
        assert!(xml.contains("会议 &lt;提醒&gt;"));
        assert!(xml.contains("A &amp; B"));
        assert!(xml.contains("<audio silent=\"true\"/>"));
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };
        let document = XmlDocument::new().unwrap();
        document.LoadXml(&HSTRING::from(xml)).unwrap();
        let root = document.DocumentElement().unwrap();
        assert_eq!(
            root.GetAttribute(&HSTRING::from("activationType")).unwrap(),
            "protocol"
        );
        assert_eq!(
            root.GetAttribute(&HSTRING::from("launch")).unwrap(),
            "https://example.com/?a=1&b=\"中文\""
        );
    }
}

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

pub fn open_url(url: &str) -> Result<()> {
    let operation = wide("open");
    let target = wide(url);
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    anyhow::ensure!(
        result.0 as usize > 32,
        "Windows 无法打开通知链接（ShellExecuteW={}）",
        result.0 as usize
    );
    Ok(())
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

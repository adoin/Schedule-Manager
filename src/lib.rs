pub mod calendar;
pub mod models;
pub mod repository;

#[cfg(feature = "desktop")]
pub mod api_client;
#[cfg(feature = "desktop")]
pub mod desktop;
#[cfg(feature = "desktop")]
pub mod widget;
#[cfg(all(feature = "desktop", target_os = "windows"))]
pub mod windows_notifications;

pub const DEFAULT_API_BASE_URL: &str = "https://novel.emssion.com/schedule";

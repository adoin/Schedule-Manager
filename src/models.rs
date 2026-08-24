use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEvent {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub location: String,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub all_day: bool,
    #[serde(default = "default_color")]
    pub color: String,
    #[serde(default)]
    pub recurrence_rule: String,
    #[serde(default = "default_reminders")]
    pub reminder_minutes: Vec<i64>,
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub updated_seq: i64,
    pub updated_at: DateTime<Utc>,
}

fn default_timezone() -> String {
    "Asia/Shanghai".into()
}

fn default_color() -> String {
    "#6878d6".into()
}

fn default_reminders() -> Vec<i64> {
    vec![0]
}

impl CalendarEvent {
    pub fn draft(start_at: DateTime<Utc>, end_at: DateTime<Utc>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: String::new(),
            notes: String::new(),
            location: String::new(),
            start_at,
            end_at,
            timezone: default_timezone(),
            all_day: false,
            color: default_color(),
            recurrence_rule: String::new(),
            reminder_minutes: default_reminders(),
            completed: false,
            deleted: false,
            version: 0,
            updated_seq: 0,
            updated_at: Utc::now(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Holiday {
    pub date: String,
    pub name: String,
    pub is_off_day: bool,
    #[serde(default)]
    pub source_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserProfile {
    pub id: String,
    pub email: String,
    pub display_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthData {
    pub token: String,
    pub user: UserProfile,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiEnvelope<T> {
    pub code: i32,
    pub message: String,
    pub data: T,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPullData {
    pub events: Vec<CalendarEvent>,
    pub holidays: Vec<Holiday>,
    pub cursor: i64,
}

use crate::{
    calendar::{event_occurs_on, occurrence_start},
    models::{CalendarEvent, Holiday},
};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Asia::Shanghai;
use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;

pub struct LocalRepository {
    connection: Connection,
}

impl LocalRepository {
    pub fn open() -> Result<Self> {
        let project = ProjectDirs::from("com", "Emssion", "ScheduleManager")
            .context("cannot resolve application data directory")?;
        std::fs::create_dir_all(project.data_local_dir())?;
        Self::open_at(project.data_local_dir().join("schedule.db"))
    }

    pub fn open_at(path: PathBuf) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let repository = Self { connection };
        repository.migrate()?;
        Ok(repository)
    }

    fn migrate(&self) -> Result<()> {
        self.connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS events (
                id TEXT PRIMARY KEY,
                payload TEXT NOT NULL,
                start_at TEXT NOT NULL,
                end_at TEXT NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0,
                version INTEGER NOT NULL DEFAULT 0,
                updated_seq INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_range ON events(start_at, end_at, deleted);
            CREATE TABLE IF NOT EXISTS holidays (
                date TEXT PRIMARY KEY,
                payload TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS reminder_deliveries (
                event_id TEXT NOT NULL,
                event_version INTEGER NOT NULL,
                offset_minutes INTEGER NOT NULL,
                delivered_at TEXT NOT NULL,
                PRIMARY KEY(event_id, event_version, offset_minutes)
            );
            CREATE TABLE IF NOT EXISTS reminder_deliveries_v2 (
                event_id TEXT NOT NULL,
                event_version INTEGER NOT NULL,
                occurrence_at TEXT NOT NULL,
                offset_minutes INTEGER NOT NULL,
                delivered_at TEXT NOT NULL,
                PRIMARY KEY(event_id, event_version, occurrence_at, offset_minutes)
            );
            "#,
        )?;
        Ok(())
    }

    pub fn upsert_event(&self, event: &CalendarEvent) -> Result<()> {
        let payload = serde_json::to_string(event)?;
        self.connection.execute(
            r#"INSERT INTO events(id,payload,start_at,end_at,deleted,version,updated_seq,updated_at)
               VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
               ON CONFLICT(id) DO UPDATE SET
                 payload=excluded.payload,start_at=excluded.start_at,end_at=excluded.end_at,
                 deleted=excluded.deleted,version=excluded.version,updated_seq=excluded.updated_seq,
                 updated_at=excluded.updated_at"#,
            params![
                event.id,
                payload,
                event.start_at.to_rfc3339(),
                event.end_at.to_rfc3339(),
                event.deleted as i32,
                event.version,
                event.updated_seq,
                event.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn event(&self, id: &str) -> Result<Option<CalendarEvent>> {
        let payload: Option<String> = self
            .connection
            .query_row("SELECT payload FROM events WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        payload
            .map(|value| serde_json::from_str(&value).context("invalid stored event"))
            .transpose()
    }

    pub fn events_between(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<CalendarEvent>> {
        let mut statement = self.connection.prepare(
            "SELECT payload FROM events WHERE deleted=0 AND start_at < ?2 AND end_at >= ?1 ORDER BY start_at",
        )?;
        let rows = statement.query_map(params![start.to_rfc3339(), end.to_rfc3339()], |row| {
            row.get::<_, String>(0)
        })?;
        rows.map(|row| {
            let payload = row?;
            Ok(serde_json::from_str(&payload)?)
        })
        .collect()
    }

    pub fn all_pending_events(&self) -> Result<Vec<CalendarEvent>> {
        let mut statement = self
            .connection
            .prepare("SELECT payload FROM events WHERE updated_seq=0 ORDER BY updated_at")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn active_events(&self) -> Result<Vec<CalendarEvent>> {
        let mut statement = self
            .connection
            .prepare("SELECT payload FROM events WHERE deleted=0 ORDER BY start_at")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn all_events(&self) -> Result<Vec<CalendarEvent>> {
        let mut statement = self
            .connection
            .prepare("SELECT payload FROM events ORDER BY start_at")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn mark_deleted(&self, id: &str) -> Result<Option<CalendarEvent>> {
        let Some(mut event) = self.event(id)? else {
            return Ok(None);
        };
        event.deleted = true;
        event.updated_seq = 0;
        event.updated_at = Utc::now();
        self.upsert_event(&event)?;
        Ok(Some(event))
    }

    pub fn purge_event(&self, id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM reminder_deliveries_v2 WHERE event_id=?1", [id])?;
        self.connection
            .execute("DELETE FROM reminder_deliveries WHERE event_id=?1", [id])?;
        self.connection
            .execute("DELETE FROM events WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn replace_holidays(&mut self, holidays: &[Holiday]) -> Result<()> {
        let transaction = self.connection.transaction()?;
        for holiday in holidays {
            transaction.execute(
                "INSERT INTO holidays(date,payload) VALUES(?1,?2) ON CONFLICT(date) DO UPDATE SET payload=excluded.payload",
                params![holiday.date, serde_json::to_string(holiday)?],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn holidays_between(&self, start: &str, end: &str) -> Result<Vec<Holiday>> {
        let mut statement = self
            .connection
            .prepare("SELECT payload FROM holidays WHERE date>=?1 AND date<=?2 ORDER BY date")?;
        let rows = statement.query_map(params![start, end], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn due_reminders(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<(CalendarEvent, DateTime<Utc>, i64)>> {
        let events = self.active_events()?;
        let max_offset = events
            .iter()
            .flat_map(|event| event.reminder_minutes.iter().copied())
            .max()
            .unwrap_or(0);
        let today = now.with_timezone(&Shanghai).date_naive();
        let horizon_days = (max_offset.div_euclid(1_440) + 2).clamp(2, 367);
        let last_date = today + Duration::days(horizon_days);
        let holidays = self.holidays_between(&today.to_string(), &last_date.to_string())?;
        let holiday_map = holidays
            .into_iter()
            .map(|holiday| (holiday.date.clone(), holiday))
            .collect::<std::collections::HashMap<_, _>>();
        let mut due = Vec::new();
        for event in events {
            let event_date = event.start_at.with_timezone(&Shanghai).date_naive();
            let candidate_dates: Box<dyn Iterator<Item = chrono::NaiveDate>> =
                if event.recurrence_rule.is_empty() {
                    Box::new(std::iter::once(event_date))
                } else {
                    Box::new((0..=horizon_days).map(|days| today + Duration::days(days)))
                };
            for date in candidate_dates {
                let holiday = holiday_map.get(&date.to_string());
                if !event_occurs_on(&event, date, holiday) {
                    continue;
                }
                let occurrence = occurrence_start(&event, date)?;
                for offset in &event.reminder_minutes {
                    let trigger = occurrence - Duration::minutes(*offset);
                    if now < trigger || now > trigger + Duration::minutes(5) {
                        continue;
                    }
                    let delivered: bool = self.connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM reminder_deliveries_v2 WHERE event_id=?1 AND event_version=?2 AND occurrence_at=?3 AND offset_minutes=?4)",
                        params![event.id, event.version, occurrence.to_rfc3339(), offset],
                    |row| row.get(0),
                )?;
                    if !delivered {
                        due.push((event.clone(), occurrence, *offset));
                    }
                }
            }
        }
        Ok(due)
    }

    pub fn mark_reminder_delivered(
        &self,
        event_id: &str,
        event_version: i64,
        occurrence_at: DateTime<Utc>,
        offset_minutes: i64,
        delivered_at: DateTime<Utc>,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO reminder_deliveries_v2(event_id,event_version,occurrence_at,offset_minutes,delivered_at) VALUES(?1,?2,?3,?4,?5)",
            params![
                event_id,
                event_version,
                occurrence_at.to_rfc3339(),
                offset_minutes,
                delivered_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_reads_event() {
        let path =
            std::env::temp_dir().join(format!("schedule-manager-{}.db", uuid::Uuid::new_v4()));
        let repo = LocalRepository::open_at(path.clone()).unwrap();
        let start = Utc::now();
        let event = CalendarEvent::draft(start, start + Duration::hours(1));
        repo.upsert_event(&event).unwrap();
        assert_eq!(repo.event(&event.id).unwrap().unwrap().id, event.id);
        drop(repo);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recurring_event_reminders_are_deduplicated_per_occurrence() {
        use chrono::TimeZone;

        let path =
            std::env::temp_dir().join(format!("schedule-reminder-{}.db", uuid::Uuid::new_v4()));
        let repo = LocalRepository::open_at(path.clone()).unwrap();
        let occurrence = Shanghai.with_ymd_and_hms(2026, 8, 24, 10, 0, 0).unwrap();
        let mut event = CalendarEvent::draft(
            occurrence.with_timezone(&Utc),
            (occurrence + Duration::hours(1)).with_timezone(&Utc),
        );
        event.recurrence_rule = "DAILY".into();
        event.reminder_minutes = vec![10];
        repo.upsert_event(&event).unwrap();
        let now = (occurrence - Duration::minutes(10)).with_timezone(&Utc);
        assert_eq!(repo.due_reminders(now).unwrap().len(), 1);
        repo.mark_reminder_delivered(
            &event.id,
            event.version,
            occurrence.with_timezone(&Utc),
            10,
            now,
        )
        .unwrap();
        assert!(repo.due_reminders(now).unwrap().is_empty());
        drop(repo);
        let _ = std::fs::remove_file(path);
    }
}

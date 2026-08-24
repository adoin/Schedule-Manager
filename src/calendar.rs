use crate::models::{CalendarEvent, Holiday};
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc, Weekday};
use chrono_tz::Asia::Shanghai;
use lunar_lite::{SolarDate, solar_to_lunar};

const MONTH_NAMES: [&str; 12] = [
    "正月", "二月", "三月", "四月", "五月", "六月", "七月", "八月", "九月", "十月", "冬月", "腊月",
];
const DAY_NAMES: [&str; 30] = [
    "初一", "初二", "初三", "初四", "初五", "初六", "初七", "初八", "初九", "初十", "十一", "十二",
    "十三", "十四", "十五", "十六", "十七", "十八", "十九", "二十", "廿一", "廿二", "廿三", "廿四",
    "廿五", "廿六", "廿七", "廿八", "廿九", "三十",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleKind {
    Specific = 0,
    LegalRestDay = 1,
    LegalWorkday = 2,
    YearlySolar = 3,
    YearlyLunar = 4,
    Daily = 5,
}

impl ScheduleKind {
    pub fn from_index(value: i32) -> Self {
        match value {
            1 => Self::LegalRestDay,
            2 => Self::LegalWorkday,
            3 => Self::YearlySolar,
            4 => Self::YearlyLunar,
            5 => Self::Daily,
            _ => Self::Specific,
        }
    }

    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Specific => "具体日期与时间",
            Self::LegalRestDay => "每个法定休息日",
            Self::LegalWorkday => "每个法定工作日",
            Self::YearlySolar => "每年公历某一天",
            Self::YearlyLunar => "每年农历某一天",
            Self::Daily => "每天某个时刻",
        }
    }
}

pub fn schedule_kind(event: &CalendarEvent) -> ScheduleKind {
    let rule = event.recurrence_rule.as_str();
    if rule == "LEGAL_REST_DAY" {
        ScheduleKind::LegalRestDay
    } else if rule == "LEGAL_WORKDAY" {
        ScheduleKind::LegalWorkday
    } else if rule.starts_with("YEARLY_SOLAR:") {
        ScheduleKind::YearlySolar
    } else if rule.starts_with("YEARLY_LUNAR:") {
        ScheduleKind::YearlyLunar
    } else if rule == "DAILY" {
        ScheduleKind::Daily
    } else {
        ScheduleKind::Specific
    }
}

pub fn yearly_solar_parts(event: &CalendarEvent) -> (u32, u32) {
    event
        .recurrence_rule
        .strip_prefix("YEARLY_SOLAR:")
        .and_then(|value| value.split_once('-'))
        .and_then(|(month, day)| Some((month.parse().ok()?, day.parse().ok()?)))
        .unwrap_or_else(|| {
            let date = event.start_at.with_timezone(&Shanghai).date_naive();
            (date.month(), date.day())
        })
}

pub fn yearly_lunar_parts(event: &CalendarEvent) -> (u8, u8, bool) {
    event
        .recurrence_rule
        .strip_prefix("YEARLY_LUNAR:")
        .and_then(|value| {
            let mut parts = value.split(':');
            let (month, day) = parts.next()?.split_once('-')?;
            Some((
                month.parse().ok()?,
                day.parse().ok()?,
                parts.next().is_some_and(|value| value == "leap"),
            ))
        })
        .unwrap_or_else(|| {
            lunar_parts(event.start_at.with_timezone(&Shanghai).date_naive())
                .unwrap_or((1, 1, false))
        })
}

pub fn lunar_parts(date: NaiveDate) -> Option<(u8, u8, bool)> {
    let lunar = solar_to_lunar(SolarDate {
        year: date.year(),
        month: date.month() as u8,
        day: date.day() as u8,
    })
    .ok()?;
    Some((lunar.month, lunar.day, lunar.is_leap_month))
}

pub fn lunar_date_label(month: u8, day: u8, leap: bool) -> String {
    let month_name = MONTH_NAMES
        .get(month.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("正月");
    let day_name = DAY_NAMES
        .get(day.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("初一");
    format!(
        "农历{}{}{}",
        if leap { "闰" } else { "" },
        month_name,
        day_name
    )
}

pub fn is_legal_rest_day(date: NaiveDate, holiday: Option<&Holiday>) -> bool {
    holiday
        .map(|item| item.is_off_day)
        .unwrap_or_else(|| matches!(date.weekday(), Weekday::Sat | Weekday::Sun))
}

pub fn event_occurs_on(event: &CalendarEvent, date: NaiveDate, holiday: Option<&Holiday>) -> bool {
    if event.deleted {
        return false;
    }
    let anchor = event.start_at.with_timezone(&Shanghai).date_naive();
    match schedule_kind(event) {
        ScheduleKind::Specific => date == anchor,
        ScheduleKind::LegalRestDay => date >= anchor && is_legal_rest_day(date, holiday),
        ScheduleKind::LegalWorkday => date >= anchor && !is_legal_rest_day(date, holiday),
        ScheduleKind::YearlySolar => {
            let (month, day) = yearly_solar_parts(event);
            date.month() == month && date.day() == day
        }
        ScheduleKind::YearlyLunar => {
            let expected = yearly_lunar_parts(event);
            lunar_parts(date).is_some_and(|actual| actual == expected)
        }
        ScheduleKind::Daily => date >= anchor,
    }
}

pub fn occurrence_start(event: &CalendarEvent, date: NaiveDate) -> Result<DateTime<Utc>> {
    if schedule_kind(event) == ScheduleKind::Specific {
        return Ok(event.start_at);
    }
    let local = event.start_at.with_timezone(&Shanghai);
    let time = if event.all_day {
        chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()
    } else {
        local.time()
    };
    Ok(Shanghai
        .from_local_datetime(&date.and_time(time))
        .single()
        .context("该日程发生时间无效")?
        .with_timezone(&Utc))
}

pub fn lunar_label(date: NaiveDate) -> String {
    let solar = SolarDate {
        year: date.year(),
        month: date.month() as u8,
        day: date.day() as u8,
    };
    let Ok(lunar) = solar_to_lunar(solar) else {
        return String::new();
    };
    if lunar.day == 1 {
        let leap = if lunar.is_leap_month { "闰" } else { "" };
        let month = MONTH_NAMES
            .get(lunar.month.saturating_sub(1) as usize)
            .copied()
            .unwrap_or("");
        format!("{leap}{month}")
    } else {
        DAY_NAMES
            .get(lunar.day.saturating_sub(1) as usize)
            .copied()
            .unwrap_or("")
            .into()
    }
}

pub fn lunar_festival(date: NaiveDate) -> Option<&'static str> {
    let lunar = solar_to_lunar(SolarDate {
        year: date.year(),
        month: date.month() as u8,
        day: date.day() as u8,
    })
    .ok()?;
    if lunar.is_leap_month {
        return None;
    }
    match (lunar.month, lunar.day) {
        (1, 1) => Some("春节"),
        (1, 15) => Some("元宵"),
        (5, 5) => Some("端午"),
        (7, 7) => Some("七夕"),
        (8, 15) => Some("中秋"),
        (9, 9) => Some("重阳"),
        (12, 8) => Some("腊八"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_new_year_is_first_lunar_day() {
        let date = NaiveDate::from_ymd_opt(2026, 2, 17).unwrap();
        assert_eq!(lunar_label(date), "正月");
        assert_eq!(lunar_festival(date), Some("春节"));
    }

    #[test]
    fn makeup_weekend_is_a_legal_workday() {
        let date = NaiveDate::from_ymd_opt(2026, 2, 14).unwrap();
        let holiday = Holiday {
            date: date.to_string(),
            name: "春节调休".into(),
            is_off_day: false,
            source_url: String::new(),
        };
        assert!(!is_legal_rest_day(date, Some(&holiday)));
    }

    #[test]
    fn yearly_lunar_rule_matches_chinese_new_year() {
        let date = NaiveDate::from_ymd_opt(2026, 2, 17).unwrap();
        let start = Shanghai
            .with_ymd_and_hms(2025, 1, 29, 9, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let mut event = CalendarEvent::draft(start, start + chrono::Duration::hours(1));
        event.recurrence_rule = "YEARLY_LUNAR:01-01:regular".into();
        assert!(event_occurs_on(&event, date, None));
    }
}

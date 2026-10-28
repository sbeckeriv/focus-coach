//! `work_hours`: when check-ins may fire and when the workday ends.
//! Written like "9-17 Mon-Fri", "9:30-17:30 Mon,Tue,Thu" or "8-16" (every day).

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Local, NaiveTime, Weekday};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkHours {
    /// As written, for messages and the prompt.
    text: String,
    pub start: NaiveTime,
    pub end: NaiveTime,
    /// Indexed by days from Monday.
    pub days: [bool; 7],
}

impl WorkHours {
    /// `None` for an empty string: no limit.
    pub fn parse(text: &str) -> Result<Option<Self>> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let (times, days) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let (start, end) = times.split_once('-').with_context(|| format!("work_hours {text:?}: expected e.g. \"9-17 Mon-Fri\""))?;
        let (start, end) = (time(start)?, time(end)?);
        if end <= start {
            bail!("work_hours {text:?}: the end must be after the start");
        }
        let days = if days.trim().is_empty() { [true; 7] } else { weekdays(days.trim())? };
        Ok(Some(Self { text: text.to_string(), start, end, days }))
    }

    pub fn is_work_day(&self, at: DateTime<Local>) -> bool {
        self.days[at.weekday().num_days_from_monday() as usize]
    }

    pub fn contains(&self, at: DateTime<Local>) -> bool {
        let t = at.time();
        self.is_work_day(at) && t >= self.start && t < self.end
    }
}

impl std::fmt::Display for WorkHours {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Reads `work_hours` from config: a string like "9-17 Mon-Fri", or "" for any time.
pub(crate) fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<WorkHours>, D::Error> {
    let text = <String as serde::Deserialize>::deserialize(d)?;
    WorkHours::parse(&text).map_err(serde::de::Error::custom)
}

fn time(s: &str) -> Result<NaiveTime> {
    let s = s.trim();
    let (h, m) = s.split_once(':').unwrap_or((s, "0"));
    let (h, m): (u32, u32) = (h.parse().with_context(|| format!("bad hour {s:?}"))?, m.parse().with_context(|| format!("bad minute {s:?}"))?);
    // "17" or "24" as an end means the top of that hour.
    if h == 24 && m == 0 {
        return Ok(NaiveTime::from_hms_opt(23, 59, 59).unwrap());
    }
    NaiveTime::from_hms_opt(h, m, 0).with_context(|| format!("bad time {s:?}"))
}

fn weekdays(s: &str) -> Result<[bool; 7]> {
    let mut days = [false; 7];
    for part in s.split(',') {
        let part = part.trim();
        let (from, to) = part.split_once('-').unwrap_or((part, part));
        let (from, to) = (weekday(from)?, weekday(to)?);
        let (mut d, last) = (from.num_days_from_monday(), to.num_days_from_monday());
        // Wraps, so "Sat-Mon" works.
        loop {
            days[d as usize] = true;
            if d == last {
                break;
            }
            d = (d + 1) % 7;
        }
    }
    Ok(days)
}

fn weekday(s: &str) -> Result<Weekday> {
    s.trim().parse::<Weekday>().map_err(|_| anyhow::anyhow!("bad weekday {s:?}"))
}

/// When a snooze ends: a length from `now` ("45m", "1h", "1h30m", "90 minutes"; a bare
/// number is minutes) or a time of day ("15:00", "3pm", "3:30pm"), today or, once that's
/// past, tomorrow.
pub fn parse_until(text: &str, now: DateTime<Local>) -> Result<DateTime<Local>> {
    let t = text.trim().to_lowercase().replace(' ', "");
    if let Some(minutes) = minutes_in(&t) {
        return Ok(now + chrono::Duration::minutes(minutes));
    }
    let Some(time) = time_of_day(&t) else {
        bail!("snooze until when? e.g. 45m, 1h30m, 15:00 or 3pm (got {text:?})");
    };
    let mut day = now.date_naive();
    if time <= now.time() {
        day = day.succ_opt().context("no tomorrow")?;
    }
    day.and_time(time).and_local_timezone(Local).earliest().context("that time doesn't exist today")
}

/// "1h30m", "45m", "2h", "90"; None for anything else.
fn minutes_in(t: &str) -> Option<i64> {
    if let Ok(n) = t.parse::<i64>() {
        return (n > 0).then_some(n);
    }
    let mut t = t.to_string();
    for (long, short) in [("hours", "h"), ("hour", "h"), ("hrs", "h"), ("hr", "h"), ("minutes", "m"), ("minute", "m"), ("mins", "m"), ("min", "m")] {
        t = t.replace(long, short);
    }
    let (hours, rest) = match t.split_once('h') {
        Some((h, rest)) => (h.parse::<i64>().ok()?, rest),
        None => (0, t.as_str()),
    };
    let minutes = match rest {
        "" => 0,
        m => m.strip_suffix('m')?.parse::<i64>().ok()?,
    };
    let total = hours * 60 + minutes;
    (total > 0).then_some(total)
}

/// "15:00", "9:30", "3pm", "3:30pm", "12am".
fn time_of_day(t: &str) -> Option<NaiveTime> {
    let (clock, half) = match (t.strip_suffix("am"), t.strip_suffix("pm")) {
        (Some(c), _) => (c, Some(0)),
        (_, Some(c)) => (c, Some(12)),
        _ => (t, None),
    };
    let (h, m) = clock.split_once(':').unwrap_or((clock, "0"));
    let (mut h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
    if let Some(offset) = half {
        if !(1..=12).contains(&h) {
            return None;
        }
        h = h % 12 + offset;
    } else if !clock.contains(':') {
        return None; // A bare number is minutes, handled above.
    }
    NaiveTime::from_hms_opt(h, m, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn snooze_lengths_and_times() {
        let now = Local.with_ymd_and_hms(2026, 10, 28, 14, 10, 0).unwrap();
        let at = |text: &str| parse_until(text, now).map(|t| t.format("%a %H:%M").to_string()).map_err(|e| e.to_string());
        for (text, want) in [
            ("45m", "Wed 14:55"),
            ("1h", "Wed 15:10"),
            ("1h30m", "Wed 15:40"),
            ("1 hour 30 mins", "Wed 15:40"),
            ("90", "Wed 15:40"),
            ("15:00", "Wed 15:00"),
            ("3pm", "Wed 15:00"),
            ("3:30 PM", "Wed 15:30"),
            ("9:00", "Thu 09:00"),
            ("12am", "Thu 00:00"),
        ] {
            assert_eq!(at(text).as_deref(), Ok(want), "{text}");
        }
        for bad in ["soon", "0", "13pm", "25:00", "1h30"] {
            assert!(at(bad).is_err(), "{bad} should fail");
        }
    }

    fn at(day: u32, h: u32, m: u32) -> DateTime<Local> {
        // 2026-10-19 is a Monday.
        Local.with_ymd_and_hms(2026, 10, 19 + day, h, m, 0).unwrap()
    }

    #[test]
    fn parses_the_usual_forms() {
        let w = WorkHours::parse("9-17 Mon-Fri").unwrap().unwrap();
        assert!(w.contains(at(0, 9, 0)) && w.contains(at(4, 16, 59)));
        assert!(!w.contains(at(0, 17, 0)), "the end is exclusive");
        assert!(!w.contains(at(0, 8, 59)) && !w.contains(at(5, 12, 0)), "weekends are off");

        let w = WorkHours::parse("9:30-17:30 Mon,Wed,Fri").unwrap().unwrap();
        assert!(!w.contains(at(0, 9, 15)) && w.contains(at(0, 17, 15)));
        assert!(!w.contains(at(1, 12, 0)) && w.contains(at(2, 12, 0)));

        assert!(WorkHours::parse("8-16").unwrap().unwrap().contains(at(6, 10, 0)), "no days means every day");
        assert!(WorkHours::parse("10-14 Sat-Mon").unwrap().unwrap().contains(at(6, 11, 0)), "day ranges wrap");
        assert_eq!(WorkHours::parse("  ").unwrap(), None);
    }

    #[test]
    fn rejects_nonsense() {
        assert!(WorkHours::parse("nine to five").is_err());
        assert!(WorkHours::parse("17-9").is_err());
        assert!(WorkHours::parse("9-17 Funday").is_err());
    }
}

//! `work_hours`: when check-ins may fire and when the workday ends.
//! Written like "9-17 Mon-Fri", "9:30-17:30 Mon,Tue,Thu" or "8-16" (every day).

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Local, NaiveTime, Weekday};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkHours {
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
        Ok(Some(Self { start, end, days }))
    }

    pub fn is_work_day(&self, at: DateTime<Local>) -> bool {
        self.days[at.weekday().num_days_from_monday() as usize]
    }

    pub fn contains(&self, at: DateTime<Local>) -> bool {
        let t = at.time();
        self.is_work_day(at) && t >= self.start && t < self.end
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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

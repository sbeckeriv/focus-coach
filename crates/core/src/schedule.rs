//! Five-field cron expressions (`minute hour day-of-month month day-of-week`).

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, Local, Timelike};

/// A parsed schedule. Reads and writes as its expression, so config and state.json
/// hold plain text and a bad expression fails when it's read.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Cron {
    expr: String,
    minutes: Vec<bool>,
    hours: Vec<bool>,
    days: Vec<bool>,
    months: Vec<bool>,
    weekdays: Vec<bool>,
    days_any: bool,
    weekdays_any: bool,
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Self> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        let [min, hour, dom, mon, dow] = fields[..] else {
            bail!("cron needs 5 fields (minute hour day month weekday), got {:?}", expr);
        };
        let mut weekdays = parse_field(dow, 0, 7).context("weekday field")?;
        if weekdays[7] {
            weekdays[0] = true;
        }
        weekdays.truncate(7);
        Ok(Self {
            expr: fields.join(" "),
            minutes: parse_field(min, 0, 59).context("minute field")?,
            hours: parse_field(hour, 0, 23).context("hour field")?,
            days: parse_field(dom, 1, 31).context("day field")?,
            months: parse_field(mon, 1, 12).context("month field")?,
            weekdays,
            days_any: dom == "*",
            weekdays_any: dow == "*",
        })
    }

    pub fn as_str(&self) -> &str {
        &self.expr
    }

    fn matches(&self, t: &DateTime<Local>) -> bool {
        let day = self.days[t.day() as usize];
        let weekday = self.weekdays[t.weekday().num_days_from_sunday() as usize];
        // Standard cron: when both day fields are restricted, either may match.
        let day_ok = match (self.days_any, self.weekdays_any) {
            (true, true) => true,
            (true, false) => weekday,
            (false, true) => day,
            (false, false) => day || weekday,
        };
        self.minutes[t.minute() as usize]
            && self.hours[t.hour() as usize]
            && self.months[t.month() as usize]
            && day_ok
    }

    /// The first matching minute strictly after `after`, searching up to a year out.
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let mut t = after.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        for _ in 0..366 * 24 * 60 {
            if self.matches(&t) {
                return Some(t);
            }
            t += Duration::minutes(1);
        }
        None
    }
}

impl std::fmt::Display for Cron {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.expr)
    }
}

impl TryFrom<String> for Cron {
    type Error = anyhow::Error;
    fn try_from(expr: String) -> Result<Self> {
        Cron::parse(&expr)
    }
}

impl From<Cron> for String {
    fn from(cron: Cron) -> String {
        cron.expr
    }
}

fn parse_field(field: &str, lo: u32, hi: u32) -> Result<Vec<bool>> {
    let mut set = vec![false; hi as usize + 1];
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().context("bad step")?),
            None => (part, 1),
        };
        if step == 0 {
            bail!("step can't be 0");
        }
        let (start, end) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse()?, b.parse()?)
        } else {
            let v: u32 = range.parse().with_context(|| format!("bad value {range:?}"))?;
            (v, if part.contains('/') { hi } else { v })
        };
        if start < lo || end > hi || start > end {
            bail!("{part:?} is outside {lo}-{hi}");
        }
        for v in (start..=end).step_by(step as usize) {
            set[v as usize] = true;
        }
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn work_hours_schedule() {
        let cron = Cron::parse("7,27,47 9-17 * * 1-5").unwrap();
        // Friday 2026-10-23 17:50 -> Monday 09:07.
        let fri = Local.with_ymd_and_hms(2026, 10, 23, 17, 50, 0).unwrap();
        let next = cron.next_after(fri).unwrap();
        assert_eq!(next.format("%a %H:%M").to_string(), "Mon 09:07");
        let tue = Local.with_ymd_and_hms(2026, 10, 20, 9, 7, 0).unwrap();
        assert_eq!(cron.next_after(tue).unwrap().minute(), 27);
    }

    #[test]
    fn steps_and_errors() {
        let cron = Cron::parse("*/20 * * * *").unwrap();
        let t = Local.with_ymd_and_hms(2026, 10, 20, 9, 41, 0).unwrap();
        assert_eq!(cron.next_after(t).unwrap().minute(), 0);
        assert!(Cron::parse("61 * * * *").is_err());
        assert!(Cron::parse("* * *").is_err());
    }
}

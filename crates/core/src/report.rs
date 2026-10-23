//! End-of-day report, written by the model over the store and the log.

use std::path::PathBuf;

use anyhow::Result;
use chrono::{Duration, Local, NaiveDate};
use serde_json::json;

use crate::daemon::App;
use crate::log::LogEntry;
use crate::provider::Tier;

const REPORT_PROMPT: &str = r#"You write the end-of-day report for a person's task list. Use markdown with exactly these sections:

## Done
Grouped by day, most recent first. Use done= dates from the task list and the done archive, and the log.
## Next
Open items in the order they should be worked, with one concrete first step for the top item.
## On hold
Held items and sections, with the reason when there is one.
## Stale
Open items that are past due or haven't come up in the log for three or more days. Write "None" if there are none.

Be brief: one line per item, keep task ids, no preamble. Use they/them for the person."#;

/// Writes today's report to `reports/YYYY-MM-DD.md` and returns its text and path.
pub(crate) async fn generate(app: &App) -> Result<(String, PathBuf)> {
    let today = Local::now().date_naive();
    let config = app.config();
    let list = app.store.load()?;
    let since = today - Duration::days(config.settings.report_lookback_days.into());
    let log = app.log.since(since);
    // Finished items move to done.md, one section per day; bring back the last week of them.
    let mut done = app.store.load_done()?;
    done.sections.retain(|s| s.name.parse::<NaiveDate>().is_ok_and(|d| d >= since));
    let prompt = format!(
        "Today is {}.\n\n<task_list>\n{}</task_list>\n\n<done_archive since=\"{since}\">\n{}</done_archive>\n\n<log since=\"{since}\">\n{}\n</log>",
        today.format("%a %Y-%m-%d"),
        list.render(),
        done.render(),
        log.join("\n\n"),
    );
    let messages = [json!({"role": "user", "content": prompt})];
    let turn = config.provider.turn(Tier::Strong, REPORT_PROMPT, &[], &messages).await?;
    let text = turn.text.trim().to_string();
    let path = app.paths.reports().join(format!("{today}.md"));
    crate::store::write_atomic(&path, &format!("# Focus report {today}\n\n{text}\n"))?;
    app.log.append(&LogEntry::new("report", format!("End-of-day report saved to {}", path.display())))?;
    // However it was asked for, today's report is done; the automatic one won't repeat it.
    app.reported(today);
    Ok((text, path))
}

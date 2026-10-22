//! End-of-day report, written by the model over the store and the log.

use std::path::PathBuf;

use anyhow::Result;
use chrono::{Duration, Local};
use serde_json::json;

use crate::daemon::App;
use crate::log::LogEntry;
use crate::provider::Tier;

const REPORT_PROMPT: &str = r#"You write the end-of-day report for a person's task list. Use markdown with exactly these sections:

## Done
Grouped by day, most recent first. Use each item's done= date and the log.
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
    let list = app.store.load()?;
    let log = app.log.since(today - Duration::days(7));
    let prompt = format!(
        "Today is {}.\n\n<task_list>\n{}</task_list>\n\n<log since=\"{}\">\n{}\n</log>",
        today.format("%a %Y-%m-%d"),
        list.render(),
        today - Duration::days(7),
        log.join("\n\n"),
    );
    let messages = [json!({"role": "user", "content": prompt})];
    let turn = app.provider.turn(Tier::Strong, REPORT_PROMPT, &[], &messages).await?;
    let text = turn.text.trim().to_string();
    let path = app.paths.reports().join(format!("{today}.md"));
    std::fs::write(&path, format!("# Focus report {today}\n\n{text}\n"))?;
    app.log.append(&LogEntry::new("report", format!("End-of-day report saved to {}", path.display())))?;
    Ok((text, path))
}

//! Append-only check-in log, kept as markdown next to the task list.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate};

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: DateTime<Local>,
    pub kind: &'static str,
    pub user_reply: Option<String>,
    pub interpretation: String,
    pub changes: Vec<String>,
}

impl LogEntry {
    pub fn new(kind: &'static str, interpretation: impl Into<String>) -> Self {
        Self {
            timestamp: Local::now(),
            kind,
            user_reply: None,
            interpretation: interpretation.into(),
            changes: Vec::new(),
        }
    }

    fn render(&self) -> String {
        let mut out = format!("\n## {} · {}\n", self.timestamp.format("%Y-%m-%d %H:%M"), self.kind);
        if let Some(reply) = &self.user_reply {
            for line in reply.lines() {
                out.push_str(&format!("> {line}\n"));
            }
        }
        if !self.interpretation.is_empty() {
            out.push_str(&format!("\n{}\n", self.interpretation.trim()));
        }
        if !self.changes.is_empty() {
            out.push('\n');
            for change in &self.changes {
                out.push_str(&format!("- {change}\n"));
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct Log {
    path: PathBuf,
}

impl Log {
    pub fn new(path: &Path) -> Self {
        Self { path: path.to_path_buf() }
    }

    pub fn append(&self, entry: &LogEntry) -> Result<()> {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        file.write_all(entry.render().as_bytes())?;
        Ok(())
    }

    fn entries(&self) -> Vec<String> {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        text.split("\n## ")
            .skip(1)
            .map(|e| format!("## {}", e.trim_end()))
            .collect()
    }

    /// The last `n` entries, oldest first, as raw markdown.
    pub fn recent(&self, n: usize) -> Vec<String> {
        let entries = self.entries();
        entries[entries.len().saturating_sub(n)..].to_vec()
    }

    /// Deletes entries older than `keep_days` (0 keeps everything). Returns how many went.
    pub fn prune(&self, keep_days: u32, today: NaiveDate) -> Result<usize> {
        if keep_days == 0 {
            return Ok(0);
        }
        let cutoff = (today - chrono::Days::new(keep_days.into())).format("%Y-%m-%d").to_string();
        let entries = self.entries();
        // Undated text (anything not under a "## YYYY-MM-DD" heading) is kept.
        let keep: Vec<&String> =
            entries.iter().filter(|e| e.get(3..13).is_none_or(|d| d.parse::<NaiveDate>().is_err() || d >= cutoff.as_str())).collect();
        let removed = entries.len() - keep.len();
        if removed > 0 {
            let text: String = keep.iter().map(|e| format!("\n{e}\n")).collect();
            crate::store::write_atomic(&self.path, &text)?;
        }
        Ok(removed)
    }

    /// Every entry dated on or after `since`.
    pub fn since(&self, since: NaiveDate) -> Vec<String> {
        let prefix = since.format("%Y-%m-%d").to_string();
        self.entries()
            .into_iter()
            .filter(|e| e.get(3..13).is_some_and(|d| d >= prefix.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_drops_old_entries_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.md");
        std::fs::write(
            &path,
            "\n## 2026-09-01 09:07 · check-in\n\nOld.\n\n## 2026-10-20 09:07 · reply\n> on it\n\nRecent.\n\n## 2026-10-22 10:00 · message\n\nToday.\n",
        )
        .unwrap();
        let log = Log::new(&path);
        let today = NaiveDate::from_ymd_opt(2026, 10, 22).unwrap();

        assert_eq!(log.prune(0, today).unwrap(), 0, "0 keeps everything");
        assert_eq!(log.prune(30, today).unwrap(), 1);
        let entries = log.entries();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].contains("> on it") && entries[1].contains("Today."), "{entries:?}");
        assert_eq!(log.prune(30, today).unwrap(), 0);

        log.append(&LogEntry::new("message", "after pruning")).unwrap();
        assert_eq!(log.entries().len(), 3, "appending still works");
    }
}

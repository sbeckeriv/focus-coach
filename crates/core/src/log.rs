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

    /// Every entry dated on or after `since`.
    pub fn since(&self, since: NaiveDate) -> Vec<String> {
        let prefix = since.format("%Y-%m-%d").to_string();
        self.entries()
            .into_iter()
            .filter(|e| e.get(3..13).is_some_and(|d| d >= prefix.as_str()))
            .collect()
    }
}

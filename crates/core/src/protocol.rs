//! Newline-delimited JSON spoken over the daemon's Unix socket. A client writes one
//! `Request` and reads `Event`s until `Done` (or forever, for `Subscribe`).

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::store::TaskList;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Tasks,
    /// Free text: a check-in reply, a command, a link.
    Say { text: String },
    /// Start or restart the check-ins, optionally with bootstrap instructions.
    Start { text: Option<String> },
    Pause,
    Resume,
    Next,
    Add { url: String },
    /// Check a task off, or reopen it if it's done. No model call.
    Toggle { id: String },
    /// A coach tool call from `focus mcp`, run inside the active claude-code turn.
    Tool { turn: u64, name: String, input: serde_json::Value },
    Report,
    Subscribe,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub paused: bool,
    pub schedule: String,
    pub next_checkin: Option<DateTime<Local>>,
    pub awaiting_reply: bool,
    pub missed: u32,
    pub busy: bool,
    pub top: Option<String>,
    pub open: usize,
    /// The Claude Code session the coach is in, for provider = "claude-code".
    #[serde(default)]
    pub session: Option<String>,
}

impl Status {
    pub fn summary(&self) -> String {
        let timer = if self.paused {
            "paused until your next message".to_string()
        } else {
            match self.next_checkin {
                Some(t) => format!("next check-in {}", t.format("%a %H:%M")),
                None => "no upcoming check-in".to_string(),
            }
        };
        let top = self.top.as_deref().unwrap_or("nothing open");
        format!("{} open · top: {top} · {timer}", self.open)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    CheckIn { text: String },
    Reply { turn: u64, text: String },
    Question { turn: u64, text: String },
    Preview { turn: u64, kind: String, content: String },
    Changed { turn: u64, summary: String },
    Report { turn: u64, text: String, path: String },
    Error { turn: u64, text: String },
    /// The answer to `Request::Tool`.
    ToolResult { content: String, is_error: bool },
    /// Every turn ends with this, even after an error.
    TurnDone { turn: u64 },
    Status { status: Status },
    Tasks { list: TaskList },
    Ok { text: String },
    Done,
}

impl Event {
    /// The turn this event belongs to; 0 for events outside any turn.
    pub fn turn(&self) -> u64 {
        match self {
            Event::Reply { turn, .. }
            | Event::Question { turn, .. }
            | Event::Preview { turn, .. }
            | Event::Changed { turn, .. }
            | Event::Report { turn, .. }
            | Event::Error { turn, .. }
            | Event::TurnDone { turn } => *turn,
            _ => 0,
        }
    }
}

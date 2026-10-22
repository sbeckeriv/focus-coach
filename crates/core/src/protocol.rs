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
    /// Fetch a link and file it, through the coach.
    Add { url: String },
    /// Add a task straight to the list, with no model call. `due` takes YYYY-MM-DD,
    /// "today", "tomorrow" or a weekday name.
    New { title: String, section: Option<String>, due: Option<String> },
    /// Approve a proposed outgoing action; the coach then carries it out.
    Approve { id: String },
    Reject { id: String },
    /// Run a check-in now, with the progress check first if one is set.
    CheckIn,
    /// Check a task off, or reopen it if it's done. No model call.
    Toggle { id: String },
    /// A coach tool call from `focus mcp`, run inside the active claude-code turn.
    Tool { turn: u64, name: String, input: serde_json::Value },
    Report,
    /// Stream every event. `notifies`: this client shows check-in notifications itself,
    /// so the daemon doesn't send its own.
    Subscribe {
        #[serde(default)]
        notifies: bool,
    },
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
    /// Outgoing actions waiting for the person's approval.
    #[serde(default)]
    pub pending: Vec<Proposal>,
}

/// An outgoing action the coach wants to take: a message, a comment, a draft.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    /// e.g. "slack message", "pr comment", "email draft".
    pub kind: String,
    /// Where it goes: a channel, a PR URL, a person.
    pub target: String,
    pub content: String,
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
        let mut out = format!("{} open · top: {top} · {timer}", self.open);
        if !self.pending.is_empty() {
            out.push_str(&format!(" · {} awaiting approval", self.pending.len()));
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    CheckIn { text: String },
    /// A note to self the coach sent; front ends that notify show it like a check-in.
    Note { text: String },
    Reply { turn: u64, text: String },
    Question { turn: u64, text: String },
    Preview { turn: u64, kind: String, content: String },
    /// The coach proposed an outgoing action and is waiting for approval.
    Proposal { turn: u64, proposal: Proposal },
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
            | Event::Proposal { turn, .. }
            | Event::Changed { turn, .. }
            | Event::Report { turn, .. }
            | Event::Error { turn, .. }
            | Event::TurnDone { turn } => *turn,
            _ => 0,
        }
    }
}

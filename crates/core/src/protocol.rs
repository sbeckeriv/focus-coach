//! Newline-delimited JSON spoken over the daemon's Unix socket. A client writes one
//! `Request` and reads `Event`s until `Done` (or forever, for `Subscribe`).

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::store::{TaskId, TaskList};

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
    Approve { id: ActionId },
    Reject { id: ActionId },
    /// Run a check-in now, with the progress check first if one is set.
    CheckIn,
    /// Add a note to a task for context. No model call.
    Note { id: TaskId, text: String },
    /// Check a task off, or reopen it if it's done. No model call.
    Toggle { id: TaskId },
    /// A coach tool call from `focus mcp`, run inside the active claude-code turn.
    Tool { turn: TurnId, name: String, input: serde_json::Value },
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
    /// The top item's id, so front ends can mark it without re-deriving the order.
    #[serde(default)]
    pub top_id: Option<TaskId>,
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
    pub id: ActionId,
    /// e.g. "slack message", "pr comment", "email draft".
    pub kind: String,
    /// Where it goes: a channel, a PR URL, a person.
    pub target: String,
    pub content: String,
}

/// An outgoing action's id, like "a1": what `/approve` and `/reject` name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActionId(String);

impl ActionId {
    pub fn numbered(n: u32) -> Self {
        ActionId(format!("a{n}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(&self.0)
    }
}

impl std::str::FromStr for ActionId {
    type Err = std::convert::Infallible;
    /// "a1", "1" and "#1" are the same action.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().trim_start_matches('#');
        Ok(if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) { ActionId(format!("a{s}")) } else { ActionId(s.to_string()) })
    }
}

impl From<&str> for ActionId {
    fn from(s: &str) -> Self {
        s.parse().expect("infallible")
    }
}

impl PartialEq<&str> for ActionId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl Proposal {
    /// "a1: pr comment to https://…", how every front end names it.
    pub fn headline(&self) -> String {
        format!("{}: {} to {}", self.id, self.kind, self.target)
    }
}

/// A single word that's a URL, as opposed to a task title.
pub fn is_link(text: &str) -> bool {
    let text = text.trim();
    !text.contains(char::is_whitespace) && (text.starts_with("http://") || text.starts_with("https://"))
}

impl Request {
    /// What "add" means for every front end: a lone link goes to the coach to fetch and
    /// file; anything else is a title added as is.
    pub fn add(text: &str, section: Option<String>, due: Option<String>) -> Request {
        if is_link(text) && section.is_none() && due.is_none() {
            Request::Add { url: text.trim().to_string() }
        } else {
            Request::New { title: text.trim().to_string(), section, due }
        }
    }
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

/// Names one turn: every event a turn emits carries it, so a client can pick its own
/// events out of the broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub u64);

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for TurnId {
    type Err = std::num::ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(TurnId)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    CheckIn { text: String },
    /// A note to self the coach sent; front ends that notify show it like a check-in.
    Note { text: String },
    Reply { turn: TurnId, text: String },
    Question { turn: TurnId, text: String },
    Preview { turn: TurnId, kind: String, content: String },
    /// The coach proposed an outgoing action and is waiting for approval.
    Proposal { turn: TurnId, proposal: Proposal },
    /// A list change: by a turn, or by hand (a checkbox, `focus add`) with no turn.
    Changed { turn: Option<TurnId>, summary: String },
    Report { turn: TurnId, text: String, path: String },
    /// A failure in a turn, or the answer to a request that failed outside one.
    Error { turn: Option<TurnId>, text: String },
    /// The answer to `Request::Tool`.
    ToolResult { content: String, is_error: bool },
    /// Every turn ends with this, even after an error.
    TurnDone { turn: TurnId },
    Status { status: Status },
    Tasks { list: TaskList },
    Ok { text: String },
    Done,
}

impl Event {
    /// The turn this event belongs to, if any.
    pub fn turn(&self) -> Option<TurnId> {
        match self {
            Event::Reply { turn, .. }
            | Event::Question { turn, .. }
            | Event::Preview { turn, .. }
            | Event::Proposal { turn, .. }
            | Event::Report { turn, .. }
            | Event::TurnDone { turn } => Some(*turn),
            Event::Changed { turn, .. } | Event::Error { turn, .. } => *turn,
            _ => None,
        }
    }

    /// An error outside any turn: the direct answer to a request.
    pub fn error(text: impl Into<String>) -> Event {
        Event::Error { turn: None, text: text.into() }
    }
}

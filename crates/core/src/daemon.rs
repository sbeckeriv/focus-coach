//! The always-on process: owns the single check-in schedule, runs every input
//! through the interpreter one at a time, and serves front ends over a Unix socket.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast, watch};

use crate::config::{Notifier, Paths, Settings};
use crate::interpreter::{self, InputKind};
use crate::log::{Log, LogEntry};
use crate::protocol::{Event, Request, Status};
use crate::provider::{Provider, ToolCall};
use crate::{report, tools};
use crate::schedule::Cron;
use crate::store::{State, Store, Task};

const RESUME_PROMPT: &str = "I'm back. Summarize where things stand.";
const RECAP_LEN: usize = 6;

pub struct App {
    pub(crate) paths: Paths,
    pub(crate) settings: Settings,
    pub(crate) provider: Provider,
    pub(crate) store: Store,
    pub(crate) log: Log,
    runtime: Mutex<Runtime>,
    events: broadcast::Sender<Event>,
    rearm: watch::Sender<u64>,
    turn_lock: tokio::sync::Mutex<()>,
    /// The running claude-code turn, which `focus mcp` tool calls write into.
    pub(crate) agent_ctx: tokio::sync::Mutex<Option<interpreter::TurnCtx>>,
    next_turn: AtomicU64,
    shutdown: Notify,
}

#[derive(Default)]
pub(crate) struct Runtime {
    pub paused: bool,
    pub schedule: String,
    pub next_checkin: Option<DateTime<Local>>,
    pub awaiting_reply: bool,
    pub missed: u32,
    pub last_checkin: Option<String>,
    pub pending_question: Option<String>,
    /// Recent (input, reply) pairs, oldest first.
    pub recap: VecDeque<(String, String)>,
    pub busy: bool,
    pub session: Option<ClaudeSession>,
}

/// The Claude Code session claude-code turns resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaudeSession {
    pub id: String,
    pub date: NaiveDate,
    /// Whether Claude Code has saved it yet, so the next turn resumes instead of creating it.
    pub started: bool,
}

/// What survives a restart. Everything else is rebuilt from the store.
#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    paused: bool,
    schedule: Option<String>,
    #[serde(default)]
    session: Option<ClaudeSession>,
}

impl App {
    fn new(paths: Paths) -> Result<Self> {
        let settings = Settings::load(&paths)?;
        let provider = Provider::from_settings(&settings.llm)?;
        let persisted: Persisted = std::fs::read_to_string(paths.state())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let runtime = Runtime {
            paused: persisted.paused,
            schedule: persisted.schedule.unwrap_or_else(|| settings.schedule.clone()),
            session: persisted.session,
            ..Default::default()
        };
        Ok(Self {
            store: Store::new(&paths.tasks()),
            log: Log::new(&paths.log()),
            paths,
            settings,
            provider,
            runtime: Mutex::new(runtime),
            events: broadcast::channel(256).0,
            rearm: watch::channel(0).0,
            turn_lock: tokio::sync::Mutex::new(()),
            agent_ctx: tokio::sync::Mutex::new(None),
            next_turn: AtomicU64::new(0),
            shutdown: Notify::new(),
        })
    }

    pub(crate) fn rt(&self) -> MutexGuard<'_, Runtime> {
        self.runtime.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn emit(&self, event: Event) {
        // No subscribers is fine: nobody is watching right now.
        let _ = self.events.send(event);
    }

    pub(crate) fn status(&self) -> Status {
        let list = self.store.load().unwrap_or_default();
        let open = list.tasks().filter(|(s, t)| !s.on_hold && t.state == State::Open).count();
        let top = list.top_open().map(|t| format!("{} ({})", t.title, t.id));
        let rt = self.rt();
        Status {
            paused: rt.paused,
            schedule: rt.schedule.clone(),
            next_checkin: rt.next_checkin,
            awaiting_reply: rt.awaiting_reply,
            missed: rt.missed,
            busy: rt.busy,
            top,
            open,
            session: matches!(self.provider, Provider::ClaudeCode(_))
                .then(|| rt.session.as_ref().map(|s| s.id.clone()))
                .flatten(),
        }
    }

    /// The session for this claude-code turn: the configured one, or today's.
    pub(crate) fn claude_session(&self) -> ClaudeSession {
        let today = Local::now().date_naive();
        let fixed = self.settings.llm.session_id.clone();
        let mut rt = self.rt();
        let current = rt.session.as_ref().is_some_and(|s| match &fixed {
            Some(id) => &s.id == id,
            None => s.date == today,
        });
        if !current {
            let id = fixed.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            rt.session = Some(ClaudeSession { id, date: today, started: false });
        }
        rt.session.clone().expect("set above")
    }

    /// Claude Code has saved the session, so later turns resume it.
    pub(crate) fn session_started(&self, id: &str) {
        if let Some(s) = self.rt().session.as_mut().filter(|s| s.id == id) {
            s.started = true;
        }
        self.persist();
    }

    fn emit_status(&self) {
        self.emit(Event::Status { status: self.status() });
    }

    fn persist(&self) {
        let state = {
            let rt = self.rt();
            Persisted { paused: rt.paused, schedule: Some(rt.schedule.clone()), session: rt.session.clone() }
        };
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            let _ = std::fs::write(self.paths.state(), json);
        }
    }

    /// Pauses check-ins until the person's next message.
    pub(crate) fn pause(&self) {
        self.rt().paused = true;
        self.persist();
        self.emit_status();
    }

    /// Unpauses and swaps in a fresh copy of the one schedule.
    pub(crate) fn resume_timers(&self) {
        {
            let mut rt = self.rt();
            rt.paused = false;
            rt.missed = 0;
            rt.awaiting_reply = false;
        }
        self.persist();
        self.rearm.send_modify(|g| *g += 1);
    }

    pub(crate) fn set_schedule(&self, expr: &str) -> Result<Option<DateTime<Local>>> {
        let cron = Cron::parse(expr)?;
        self.rt().schedule = expr.trim().to_string();
        self.persist();
        self.rearm.send_modify(|g| *g += 1);
        Ok(cron.next_after(Local::now()))
    }

    async fn handle_input(self: Arc<Self>, turn: u64, text: String, kind: InputKind) {
        let _guard = self.turn_lock.lock().await;
        let (was_reply, unpaused) = {
            let mut rt = self.rt();
            let was_reply = rt.awaiting_reply;
            rt.awaiting_reply = false;
            rt.missed = 0;
            let unpaused = kind.is_from_person() && rt.paused;
            if unpaused {
                rt.paused = false;
            }
            rt.busy = true;
            (was_reply, unpaused)
        };
        if unpaused {
            self.persist();
            self.rearm.send_modify(|g| *g += 1);
        }
        self.emit_status();

        let result = match kind {
            InputKind::Report => report::generate(&self).await.map(|(text, path)| {
                self.emit(Event::Report { turn, text, path: path.display().to_string() });
                String::new()
            }),
            _ => interpreter::run(&self, turn, &text, kind, was_reply).await,
        };
        match result {
            Ok(reply) => {
                if !reply.trim().is_empty() {
                    self.emit(Event::Reply { turn, text: reply.clone() });
                }
                let mut rt = self.rt();
                rt.recap.push_back((text, reply));
                while rt.recap.len() > RECAP_LEN {
                    rt.recap.pop_front();
                }
            }
            Err(e) => self.emit(Event::Error { turn, text: format!("{e:#}") }),
        }
        self.rt().busy = false;
        self.emit_status();
        self.emit(Event::TurnDone { turn });
    }

    async fn scheduler(self: Arc<Self>) {
        let mut rearm = self.rearm.subscribe();
        'outer: loop {
            let expr = self.rt().schedule.clone();
            let next = match Cron::parse(&expr) {
                Ok(cron) => cron.next_after(Local::now()),
                Err(e) => {
                    self.emit(Event::Error { turn: 0, text: format!("schedule {expr:?}: {e:#}") });
                    None
                }
            };
            self.rt().next_checkin = next;
            self.emit_status();
            let Some(next) = next else {
                let _ = rearm.changed().await;
                continue;
            };
            // Short sleeps against the wall clock, so a laptop waking up doesn't drift.
            loop {
                let now = Local::now();
                if now >= next {
                    break;
                }
                let wait = (next - now).to_std().unwrap_or_default().min(Duration::from_secs(30));
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = rearm.changed() => continue 'outer,
                }
            }
            // Skip check-ins the machine slept through rather than firing them late.
            if Local::now() - next < chrono::Duration::minutes(5) {
                self.checkin().await;
            }
        }
    }

    async fn checkin(&self) {
        let text = {
            let mut rt = self.rt();
            if rt.paused {
                return;
            }
            if rt.awaiting_reply {
                rt.missed += 1;
            }
            rt.awaiting_reply = true;
            let list = self.store.load().unwrap_or_default();
            let text = checkin_prompt(list.top_open(), rt.missed, self.settings.miss_threshold);
            rt.last_checkin = Some(text.clone());
            text
        };
        let _ = self.log.append(&LogEntry::new("check-in", text.clone()));
        self.emit(Event::CheckIn { text: text.clone() });
        self.emit_status();
        if self.settings.notifier == Notifier::Desktop {
            notify_desktop(&text).await;
        }
    }

    /// Runs `text` as a turn and streams that turn's events back to `out`.
    async fn turn(self: &Arc<Self>, out: &mut OwnedWriteHalf, text: String, kind: InputKind) -> Result<()> {
        let mut rx = self.events.subscribe();
        let turn = self.next_turn.fetch_add(1, Ordering::Relaxed) + 1;
        // Spawned, so the turn finishes even if this client disconnects.
        tokio::spawn(self.clone().handle_input(turn, text, kind));
        loop {
            match rx.recv().await {
                Ok(ev) if ev.turn() == turn => {
                    let done = matches!(ev, Event::TurnDone { .. });
                    write_event(out, &ev).await?;
                    if done {
                        break;
                    }
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        write_event(out, &Event::Done).await
    }

    /// Flips a task between done and open, straight from a checkbox click.
    fn toggle(&self, id: &str) -> Result<String> {
        let today = Local::now().date_naive();
        let summary = self.store.update(|list| {
            let task = list.get_mut(id)?;
            let done = task.state != State::Done;
            task.state = if done { State::Done } else { State::Open };
            task.done_on = done.then_some(today);
            if done {
                task.blocked_by = None;
            }
            Ok(format!("{} {id}: {}", if done { "done" } else { "reopened" }, task.title))
        })?;
        let mut entry = LogEntry::new("checkbox", "");
        entry.changes.push(summary.clone());
        self.log.append(&entry)?;
        self.emit(Event::Changed { turn: 0, summary: summary.clone() });
        self.emit_status();
        Ok(summary)
    }

    async fn serve(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let (read, mut out) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let Some(line) = lines.next_line().await? else { return Ok(()) };
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                write_event(&mut out, &Event::Error { turn: 0, text: format!("bad request: {e}") }).await?;
                return write_event(&mut out, &Event::Done).await;
            }
        };
        match req {
            Request::Subscribe => {
                let mut rx = self.events.subscribe();
                write_event(&mut out, &Event::Status { status: self.status() }).await?;
                loop {
                    match rx.recv().await {
                        Ok(ev) => write_event(&mut out, &ev).await?,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
            }
            Request::Status => {
                write_event(&mut out, &Event::Status { status: self.status() }).await?;
                write_event(&mut out, &Event::Done).await
            }
            Request::Tasks => {
                write_event(&mut out, &Event::Tasks { list: self.store.load()? }).await?;
                write_event(&mut out, &Event::Done).await
            }
            Request::Say { text } => self.turn(&mut out, text, InputKind::Message).await,
            Request::Start { text } => {
                self.resume_timers();
                match text.filter(|t| !t.trim().is_empty()) {
                    Some(text) => self.turn(&mut out, text, InputKind::Bootstrap).await,
                    None => self.turn(&mut out, RESUME_PROMPT.into(), InputKind::Resume).await,
                }
            }
            Request::Resume => {
                self.resume_timers();
                self.turn(&mut out, RESUME_PROMPT.into(), InputKind::Resume).await
            }
            Request::Pause => {
                self.pause();
                let text = "Check-ins paused until your next message.".to_string();
                write_event(&mut out, &Event::Ok { text }).await?;
                write_event(&mut out, &Event::Done).await
            }
            Request::Next => self.turn(&mut out, "what's next?".into(), InputKind::Message).await,
            Request::Add { url } => self.turn(&mut out, format!("{url} add to the list"), InputKind::Message).await,
            Request::Tool { turn, name, input } => {
                let mut active = self.agent_ctx.lock().await;
                let event = match active.as_mut().filter(|ctx| ctx.turn == turn) {
                    Some(ctx) => {
                        let call = ToolCall { id: String::new(), name, input };
                        let (content, is_error) = tools::execute(&self, ctx, &call).await;
                        Event::ToolResult { content, is_error }
                    }
                    None => Event::Error { turn: 0, text: format!("turn {turn} isn't running") },
                };
                drop(active);
                write_event(&mut out, &event).await?;
                write_event(&mut out, &Event::Done).await
            }
            Request::Toggle { id } => {
                match self.toggle(&id) {
                    Ok(summary) => write_event(&mut out, &Event::Changed { turn: 0, summary }).await?,
                    Err(e) => write_event(&mut out, &Event::Error { turn: 0, text: format!("{e:#}") }).await?,
                }
                write_event(&mut out, &Event::Done).await
            }
            Request::Report => {
                self.pause();
                self.turn(&mut out, "end-of-day report".into(), InputKind::Report).await
            }
            Request::Shutdown => {
                write_event(&mut out, &Event::Ok { text: "Daemon stopping.".into() }).await?;
                write_event(&mut out, &Event::Done).await?;
                self.shutdown.notify_one();
                Ok(())
            }
        }
    }
}

async fn write_event(out: &mut OwnedWriteHalf, event: &Event) -> Result<()> {
    let mut line = serde_json::to_string(event)?;
    line.push('\n');
    out.write_all(line.as_bytes()).await?;
    Ok(())
}

/// One or two lines naming the single top item. After `threshold` unanswered
/// check-ins, assume deep work or a meeting and offer a pause once; never escalate.
fn checkin_prompt(top: Option<&Task>, missed: u32, threshold: u32) -> String {
    let top = top.map(|t| format!("{} ({})", t.title, t.id));
    if missed == threshold {
        return format!(
            "No reply to the last {missed} check-ins. I'll guess deep work or a meeting. Say \"pause\" if you want quiet."
        );
    }
    if missed > threshold {
        return match top {
            Some(t) => format!("When you're back: top of the list is {t}."),
            None => "When you're back: tell me what you're on.".into(),
        };
    }
    match top {
        Some(t) => format!("What are you on? Top of the list: {t}."),
        None => "What are you on? Nothing's open on the list yet.".into(),
    }
}

async fn notify_desktop(text: &str) {
    let quoted = format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
    let script = format!("display notification {quoted} with title \"Focus Coach\"");
    let _ = tokio::process::Command::new("osascript").args(["-e", &script]).output().await;
}

/// Runs the daemon until a `Shutdown` request. Refuses to start a second copy, which
/// is what keeps check-ins to a single schedule.
pub async fn run(paths: Paths) -> Result<()> {
    paths.ensure()?;
    let socket = paths.socket();
    if UnixStream::connect(&socket).await.is_ok() {
        bail!("a focus daemon is already running ({})", socket.display());
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;

    let app = Arc::new(App::new(paths)?);
    tokio::spawn(app.clone().scheduler());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let app = app.clone();
                tokio::spawn(async move {
                    // A client hanging up mid-stream is normal.
                    let _ = app.serve(stream).await;
                });
            }
            _ = app.shutdown.notified() => break,
        }
    }
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkin_prompt_softens_after_threshold() {
        let task = Task::new("t1".into(), "Ship it".into());
        assert_eq!(checkin_prompt(Some(&task), 0, 3), "What are you on? Top of the list: Ship it (t1).");
        assert!(checkin_prompt(Some(&task), 3, 3).contains("Say \"pause\""));
        assert!(checkin_prompt(Some(&task), 5, 3).starts_with("When you're back"));
    }
}

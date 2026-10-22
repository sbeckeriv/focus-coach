//! The always-on process: owns the single check-in schedule, runs every input
//! through the interpreter one at a time, and serves front ends over a Unix socket.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDate, NaiveTime};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast, watch};

use crate::config::{Notifier, Paths, ReportAt, Settings};
use crate::interpreter::{self, InputKind};
use crate::log::{Log, LogEntry};
use crate::protocol::{ActionId, Event, Proposal, Request, Status, TurnId};
use crate::provider::{Provider, ToolCall};
use crate::schedule::Cron;
use crate::store::{self, Store, Task, TaskId};
use crate::{report, tools};

const RESUME_PROMPT: &str = "I'm back. Summarize where things stand.";
/// An automatic report this long after its time is skipped (the machine was asleep).
const REPORT_GRACE: chrono::Duration = chrono::Duration::hours(3);

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
    /// Subscribers that show notifications themselves (the menubar app).
    notifiers: AtomicUsize,
    shutdown: Notify,
}

/// The daemon's state. Fields not marked `skip` are saved to state.json and survive a
/// restart; the rest is rebuilt from the store and the clock.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Runtime {
    pub paused: bool,
    /// The one check-in schedule. Starts as focus.toml's on every start, so the config
    /// always wins; a turn can change it until the next restart. Not saved.
    #[serde(skip)]
    pub schedule: Option<Cron>,
    pub session: Option<ClaudeSession>,
    /// Outgoing actions waiting for approval.
    pub pending: Vec<Proposal>,
    next_action: u32,
    last_bootstrap: Option<NaiveDate>,
    last_report: Option<NaiveDate>,
    #[serde(skip)]
    pub next_checkin: Option<DateTime<Local>>,
    #[serde(skip)]
    pub awaiting_reply: bool,
    #[serde(skip)]
    pub missed: u32,
    #[serde(skip)]
    pub last_checkin: Option<String>,
    #[serde(skip)]
    pub pending_question: Option<String>,
    /// Recent (input, reply) pairs, oldest first.
    #[serde(skip)]
    pub recap: VecDeque<(String, String)>,
}

/// The Claude Code session claude-code turns resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaudeSession {
    pub id: String,
    pub date: NaiveDate,
    /// Whether Claude Code has saved it yet, so the next turn resumes instead of creating it.
    pub started: bool,
}

/// Decrements the notifier count when a notifying subscriber goes away.
struct NotifierGuard<'a>(&'a AtomicUsize);

impl Drop for NotifierGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl App {
    fn new(paths: Paths) -> Result<Self> {
        let settings = Settings::load(&paths)?;
        let provider = Provider::from_settings(&settings.llm)?;
        let mut runtime: Runtime = std::fs::read_to_string(paths.state())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        runtime.schedule = Some(settings.schedule.clone());
        Ok(Self {
            store: Store::new(&paths.tasks()).with_archive(&paths.done()),
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
            notifiers: AtomicUsize::new(0),
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
        let top = list.top_open();
        let rt = self.rt();
        Status {
            paused: rt.paused,
            schedule: self.schedule_of(&rt).to_string(),
            next_checkin: rt.next_checkin,
            awaiting_reply: rt.awaiting_reply,
            missed: rt.missed,
            // Every turn holds the lock, so busy can't drift from what's actually running.
            busy: self.turn_lock.try_lock().is_err(),
            top: top.map(Task::label),
            top_id: top.map(|t| t.id.clone()),
            open: list.open_count(),
            session: matches!(self.provider, Provider::ClaudeCode(_))
                .then(|| rt.session.as_ref().map(|s| s.id.clone()))
                .flatten(),
            pending: rt.pending.clone(),
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
        let json = serde_json::to_string_pretty(&*self.rt());
        if let Ok(json) = json {
            let _ = store::write_atomic(&self.paths.state(), &json);
        }
    }

    /// Today's report is written, however it was asked for.
    pub(crate) fn reported(&self, day: NaiveDate) {
        self.rt().last_report = Some(day);
        self.persist();
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
        let next = self.next_fire(&cron, Local::now());
        self.rt().schedule = Some(cron);
        self.rearm.send_modify(|g| *g += 1);
        Ok(next)
    }

    /// The current schedule.
    fn schedule_of(&self, rt: &Runtime) -> Cron {
        rt.schedule.clone().unwrap_or_else(|| self.settings.schedule.clone())
    }

    /// Records an outgoing action for the person to approve.
    pub(crate) fn propose(&self, kind: &str, target: &str, content: &str) -> Proposal {
        let proposal = {
            let mut rt = self.rt();
            rt.next_action += 1;
            let proposal = Proposal {
                id: ActionId::numbered(rt.next_action),
                kind: kind.to_string(),
                target: target.to_string(),
                content: content.to_string(),
            };
            rt.pending.push(proposal.clone());
            proposal
        };
        self.persist();
        self.emit_status();
        proposal
    }

    fn take_pending(&self, id: &ActionId) -> Result<Proposal> {
        let proposal = {
            let mut rt = self.rt();
            let index = rt.pending.iter().position(|p| p.id == *id).with_context(|| {
                format!("nothing awaiting approval as {id}")
            })?;
            rt.pending.remove(index)
        };
        self.persist();
        self.emit_status();
        Ok(proposal)
    }

    /// A note to self: shown by front ends that notify, or as a desktop notification.
    pub(crate) async fn notify(&self, text: &str) {
        self.emit(Event::Note { text: text.to_string() });
        self.notify_desktop(text).await;
    }

    /// The daemon's own notification, unless a subscriber (the menubar app) shows them.
    async fn notify_desktop(&self, text: &str) {
        if self.settings.notifier == Notifier::Desktop && self.notifiers.load(Ordering::Relaxed) == 0 {
            notify_desktop(text).await;
        }
    }

    /// Whether today's morning bootstrap still has to run.
    fn morning_due(&self) -> bool {
        !self.settings.morning_bootstrap.trim().is_empty()
            && self.rt().last_bootstrap != Some(Local::now().date_naive())
    }

    /// The next cron time inside work hours.
    fn next_fire(&self, cron: &Cron, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let mut t = cron.next_after(after)?;
        // A year ahead is plenty to find one inside work hours, or to show there's none.
        let horizon = after + chrono::Duration::days(366);
        while t <= horizon {
            if self.settings.work_hours.as_ref().is_none_or(|w| w.contains(t)) {
                return Some(t);
            }
            t = cron.next_after(t)?;
        }
        None
    }

    /// Today's automatic report time, if there is one.
    fn report_time(&self, now: DateTime<Local>) -> Option<DateTime<Local>> {
        let time = match self.settings.report_at {
            ReportAt::Off => return None,
            ReportAt::End => self.settings.work_hours.as_ref().map_or(NaiveTime::from_hms_opt(17, 0, 0)?, |w| w.end),
            ReportAt::At(time) => time,
        };
        if self.settings.work_hours.as_ref().is_some_and(|w| !w.is_work_day(now)) {
            return None;
        }
        now.date_naive().and_time(time).and_local_timezone(Local).single()
    }

    /// Runs the morning bootstrap if this is the day's first interaction, and returns its
    /// reply. The caller holds the turn lock.
    async fn morning(self: &Arc<Self>, turn: TurnId) -> Option<String> {
        if !self.morning_due() {
            return None;
        }
        self.rt().last_bootstrap = Some(Local::now().date_naive());
        self.persist();
        let prompt = self.settings.morning_bootstrap.trim().to_string();
        self.side_turn(turn, &prompt, InputKind::Bootstrap).await
    }

    /// A turn the coach starts on its own (morning bootstrap, progress check): errors
    /// are shown, not returned. The caller holds the turn lock.
    async fn side_turn(self: &Arc<Self>, turn: TurnId, prompt: &str, kind: InputKind) -> Option<String> {
        match interpreter::run(self, turn, prompt, kind, false, None).await {
            Ok(reply) => Some(reply.trim().to_string()).filter(|r| !r.is_empty()),
            Err(e) => {
                self.emit(Event::Error { turn: Some(turn), text: format!("{}: {e:#}", kind.label(false)) });
                None
            }
        }
    }

    fn new_turn(&self) -> TurnId {
        TurnId(self.next_turn.fetch_add(1, Ordering::Relaxed) + 1)
    }

    async fn handle_input(self: Arc<Self>, turn: TurnId, text: String, kind: InputKind, approved: Option<Proposal>) {
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
            if kind == InputKind::Bootstrap {
                rt.last_bootstrap = Some(Local::now().date_naive());
            }
            (was_reply, unpaused)
        };
        if unpaused || kind == InputKind::Bootstrap {
            self.persist();
        }
        if unpaused {
            self.rearm.send_modify(|g| *g += 1);
        }
        self.emit_status();
        if matches!(kind, InputKind::Message | InputKind::Resume)
            && let Some(reply) = self.morning(turn).await
        {
            self.emit(Event::Reply { turn, text: reply });
        }

        let result = match kind {
            InputKind::Report => report::generate(&self).await.map(|(text, path)| {
                self.emit(Event::Report { turn, text, path: path.display().to_string() });
                String::new()
            }),
            _ => interpreter::run(&self, turn, &text, kind, was_reply, approved).await,
        };
        // With archive_after_days = 0, what the turn just checked off goes now.
        self.housekeeping();
        match result {
            // The report is its own event, not part of the conversation.
            Ok(_) if kind == InputKind::Report => {}
            Ok(reply) => {
                if !reply.trim().is_empty() {
                    self.emit(Event::Reply { turn, text: reply.clone() });
                }
                let mut rt = self.rt();
                rt.recap.push_back((text, reply));
                while rt.recap.len() > self.settings.recent_turns {
                    rt.recap.pop_front();
                }
            }
            Err(e) => self.emit(Event::Error { turn: Some(turn), text: format!("{e:#}") }),
        }
        drop(_guard);
        self.emit_status();
        self.emit(Event::TurnDone { turn });
    }

    async fn scheduler(self: Arc<Self>) {
        let mut rearm = self.rearm.subscribe();
        'outer: loop {
            let cron = self.schedule_of(&self.rt());
            let next = self.next_fire(&cron, Local::now());
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
            let late = chrono::Duration::minutes(self.settings.late_checkin_minutes.into());
            if Local::now() - next <= late {
                self.checkin().await;
            }
        }
    }

    /// Once-a-day jobs: housekeeping when the date rolls over (yesterday's done items
    /// move out), and the automatic end-of-day report.
    async fn daily_timer(self: Arc<Self>) {
        let mut housekept = Local::now().date_naive();
        let mut first = true;
        loop {
            if !std::mem::take(&mut first) {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            let now = Local::now();
            if now.date_naive() != housekept {
                housekept = now.date_naive();
                self.housekeeping();
            }
            let due = self.report_time(now).is_some_and(|at| now >= at && now - at < REPORT_GRACE)
                && self.rt().last_report != Some(now.date_naive());
            if due {
                // Marked first, so a slow report doesn't start twice.
                self.reported(now.date_naive());
                self.clone().handle_input(self.new_turn(), "end-of-day report".into(), InputKind::Report, None).await;
            }
        }
    }

    async fn checkin(self: &Arc<Self>) {
        if self.rt().paused {
            return;
        }
        // The first check-in of the day builds the list; later ones run the progress check,
        // so the check-in already reflects what the sources show.
        let progress = self.settings.progress_check.trim().to_string();
        let mut noticed = None;
        if self.morning_due() || !progress.is_empty() {
            let turn = self.new_turn();
            let guard = self.turn_lock.lock().await;
            self.emit_status();
            match self.morning(turn).await {
                Some(reply) => self.emit(Event::Reply { turn, text: reply }),
                None if !progress.is_empty() => noticed = self.side_turn(turn, &progress, InputKind::Progress).await,
                None => {}
            }
            drop(guard);
            self.emit(Event::TurnDone { turn });
        }
        self.housekeeping();
        let text = {
            let mut rt = self.rt();
            if rt.awaiting_reply {
                rt.missed += 1;
            }
            rt.awaiting_reply = true;
            let list = self.store.load().unwrap_or_default();
            let mut text = checkin_prompt(list.top_open(), rt.missed, self.settings.miss_threshold);
            if let Some(noticed) = &noticed {
                text.push_str(&format!("\n{noticed}"));
            }
            rt.last_checkin = Some(text.clone());
            text
        };
        let _ = self.log.append(&LogEntry::new("check-in", text.clone()));
        self.emit(Event::CheckIn { text: text.clone() });
        self.emit_status();
        self.notify_desktop(&text).await;
    }

    /// Runs `text` as a turn and streams that turn's events back to `out`. `approved` is
    /// the outgoing action an `Approved` turn may carry out.
    async fn turn(
        self: &Arc<Self>,
        out: &mut OwnedWriteHalf,
        text: String,
        kind: InputKind,
        approved: Option<Proposal>,
    ) -> Result<()> {
        let mut rx = self.events.subscribe();
        let turn = self.new_turn();
        // Spawned, so the turn finishes even if this client disconnects.
        tokio::spawn(self.clone().handle_input(turn, text, kind, approved));
        loop {
            match rx.recv().await {
                Ok(ev) if ev.turn() == Some(turn) => {
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

    /// Moves old done items to done.md and trims the log. Failing here never blocks a turn.
    fn housekeeping(&self) {
        let today = Local::now().date_naive();
        match self.store.archive_done(today, self.settings.archive_after_days) {
            Ok(0) => {}
            Ok(n) => {
                let mut entry = LogEntry::new("archive", "");
                entry.changes.push(format!("moved {n} done item{} to done.md", if n == 1 { "" } else { "s" }));
                let _ = self.log.append(&entry);
            }
            Err(e) => eprintln!("archiving done items: {e:#}"),
        }
        if let Err(e) = self.log.prune(self.settings.log_keep_days, today) {
            eprintln!("trimming the log: {e:#}");
        }
    }

    /// Flips a task between done and open, straight from a checkbox click.
    fn toggle(&self, id: &TaskId) -> Result<String> {
        let today = Local::now().date_naive();
        let summary = self.store.update(|list| {
            let task = list.get_mut(id.as_str())?;
            let done = !task.is_done();
            if done { task.mark_done(today) } else { task.reopen() }
            Ok(format!("{} {id}: {}", if done { "done" } else { "reopened" }, task.title))
        })?;
        self.changed_by_hand("checkbox", &summary)?;
        Ok(summary)
    }

    /// Adds a note to a task, with no model call.
    fn add_note(&self, id: &TaskId, text: &str) -> Result<String> {
        let text = text.trim();
        if text.is_empty() {
            bail!("a note needs some text");
        }
        let title = self.store.update(|list| {
            let task = list.get_mut(id.as_str())?;
            task.add_note(text);
            Ok(task.title.clone())
        })?;
        let summary = format!("note on {id} ({title}): {text}");
        self.changed_by_hand("note", &summary)?;
        Ok(summary)
    }

    /// Adds a task straight to the list, with no model call.
    fn add_task(&self, title: &str, section: Option<&str>, due: Option<&str>) -> Result<String> {
        let title = title.trim();
        if title.is_empty() {
            bail!("a task needs a title");
        }
        let due = due.map(|d| store::parse_due(d, Local::now().date_naive())).transpose()?;
        let section = section.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(store::DEFAULT_SECTION);
        let summary = self.store.update(|list| {
            let mut task = Task::new(list.next_id(), title.to_string());
            task.due = due;
            let id = list.add(section, task);
            let due = due.map(|d| format!(", due {}", d.format("%a %m/%d"))).unwrap_or_default();
            Ok(format!("added {id} to {section}: {title}{due}"))
        })?;
        self.changed_by_hand("added", &summary)?;
        Ok(summary)
    }

    /// Logs and announces a change made without the model.
    fn changed_by_hand(&self, kind: &'static str, summary: &str) -> Result<()> {
        let mut entry = LogEntry::new(kind, "");
        entry.changes.push(summary.to_string());
        self.log.append(&entry)?;
        self.emit(Event::Changed { turn: None, summary: summary.to_string() });
        self.housekeeping();
        self.emit_status();
        Ok(())
    }

    async fn serve(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let (read, mut out) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let Some(line) = lines.next_line().await? else { return Ok(()) };
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => return answer(&mut out, Err(anyhow::anyhow!("bad request: {e}"))).await,
        };
        match req {
            Request::Subscribe { notifies } => {
                let _guard = notifies.then(|| {
                    self.notifiers.fetch_add(1, Ordering::Relaxed);
                    NotifierGuard(&self.notifiers)
                });
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
            Request::Status => answer(&mut out, Ok(Event::Status { status: self.status() })).await,
            Request::Tasks => answer(&mut out, self.store.load().map(|list| Event::Tasks { list })).await,
            Request::Say { text } => self.turn(&mut out, text, InputKind::Message, None).await,
            Request::Start { text: Some(text) } if !text.trim().is_empty() => {
                self.resume_timers();
                self.turn(&mut out, text, InputKind::Bootstrap, None).await
            }
            Request::Start { .. } | Request::Resume => {
                self.resume_timers();
                self.turn(&mut out, RESUME_PROMPT.into(), InputKind::Resume, None).await
            }
            Request::Pause => {
                self.pause();
                answer(&mut out, Ok(Event::Ok { text: "Check-ins paused until your next message.".into() })).await
            }
            Request::Next => self.turn(&mut out, "what's next?".into(), InputKind::Message, None).await,
            Request::Add { url } => self.turn(&mut out, format!("{url} add to the list"), InputKind::Message, None).await,
            Request::New { title, section, due } => {
                let added = self.add_task(&title, section.as_deref(), due.as_deref());
                answer(&mut out, added.map(|summary| Event::Changed { turn: None, summary })).await
            }
            Request::Note { id, text } => {
                answer(&mut out, self.add_note(&id, &text).map(|summary| Event::Changed { turn: None, summary })).await
            }
            Request::Toggle { id } => {
                answer(&mut out, self.toggle(&id).map(|summary| Event::Changed { turn: None, summary })).await
            }
            Request::Approve { id } => match self.take_pending(&id) {
                Ok(p) => self.turn(&mut out, format!("approve {}", p.headline()), InputKind::Approved, Some(p)).await,
                Err(e) => answer(&mut out, Err(e)).await,
            },
            Request::Reject { id } => answer(&mut out, self.reject(&id)).await,
            Request::Tool { turn, name, input } => {
                let mut active = self.agent_ctx.lock().await;
                let event = match active.as_mut().filter(|ctx| ctx.turn == turn) {
                    Some(ctx) => {
                        let call = ToolCall { id: String::new(), name, input };
                        let (content, is_error) = tools::execute(&self, ctx, &call).await;
                        Ok(Event::ToolResult { content, is_error })
                    }
                    None => Err(anyhow::anyhow!("turn {turn} isn't running")),
                };
                drop(active);
                answer(&mut out, event).await
            }
            Request::CheckIn => {
                let result = if self.rt().paused {
                    Err(anyhow::anyhow!("Check-ins are paused; resume first."))
                } else {
                    self.checkin().await;
                    Ok(Event::Ok { text: self.rt().last_checkin.clone().unwrap_or_default() })
                };
                answer(&mut out, result).await
            }
            Request::Report => {
                self.pause();
                self.turn(&mut out, "end-of-day report".into(), InputKind::Report, None).await
            }
            Request::Shutdown => {
                answer(&mut out, Ok(Event::Ok { text: "Daemon stopping.".into() })).await?;
                self.shutdown.notify_one();
                Ok(())
            }
        }
    }

    /// Drops a proposed action without sending anything.
    fn reject(&self, id: &ActionId) -> Result<Event> {
        let p = self.take_pending(id)?;
        let mut entry = LogEntry::new("rejected", "");
        entry.changes.push(format!("rejected {}", p.headline()));
        self.log.append(&entry)?;
        Ok(Event::Ok { text: format!("Dropped {}. Nothing was sent.", p.id) })
    }
}

/// A one-event answer to a request, then `Done`. Errors go back as `Event::Error`.
async fn answer(out: &mut OwnedWriteHalf, result: Result<Event>) -> Result<()> {
    let event = result.unwrap_or_else(|e| Event::error(format!("{e:#}")));
    write_event(out, &event).await?;
    write_event(out, &Event::Done).await
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
    app.housekeeping();
    tokio::spawn(app.clone().scheduler());
    tokio::spawn(app.clone().daily_timer());
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

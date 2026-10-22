//! `focus tui`: the task list beside the conversation, with an input line.
//! Plain text goes to the interpreter; `/pause`, `/next` and friends map to requests.

use std::cell::Cell;
use std::time::Duration;

use anyhow::Result;
use chrono::Local;
use focus_core::store::{State, TaskId, TaskList};
use focus_core::{Event, Paths, Request, Status, client};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event as TermEvent, KeyCode,
    KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;

const HELP: &str = "Type anything to talk to the coach (Alt+Enter for a new line). Commands: /next /pause /resume /report /add <title or url> /note [id] <text> /approve <id> /reject <id> /copy /start [sources] /quit. Click a task's mark to check it off, or its title for details. Drag the middle border or press Alt+←/→ to resize. F2 lets you select text; Ctrl+Y copies the last reply.";

enum Msg {
    Key(TermEvent),
    Daemon(Event),
    Tasks(TaskList),
    Connected(bool),
    RequestDone,
    Tick,
}

enum Entry {
    You(String),
    CheckIn(String),
    Coach(String),
    Question(String),
    Preview { kind: String, content: String },
    /// An outgoing action waiting for /approve.
    Proposal(String),
    /// Details of a clicked task.
    Task(String),
    Change(String),
    Error(String),
    Info(String),
}

struct Ui {
    paths: Paths,
    tx: mpsc::UnboundedSender<Msg>,
    input: String,
    entries: Vec<Entry>,
    tasks: TaskList,
    status: Option<Status>,
    connected: bool,
    pending: usize,
    /// Lines scrolled up from the bottom of the conversation.
    scroll: usize,
    /// The task clicked last; Enter on an empty input opens its first link.
    selected: Option<TaskId>,
    /// Where the task and coach panes were last drawn, for mouse hits.
    tasks_area: Cell<Rect>,
    chat_area: Cell<Rect>,
    /// The task pane's share of the width, in percent.
    split: u16,
    /// Dragging the border between the panes.
    dragging: bool,
    /// Rows scrolled down in the task pane.
    task_scroll: usize,
    /// Off in select mode (F2), so the terminal's own selection and copy work.
    mouse_capture: bool,
    quit: bool,
}

pub async fn run(paths: Paths) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();

    let keys = tx.clone();
    std::thread::spawn(move || {
        loop {
            if event::poll(Duration::from_millis(250)).unwrap_or(false) {
                let Ok(ev) = event::read() else { break };
                if keys.send(Msg::Key(ev)).is_err() {
                    break;
                }
            } else if keys.is_closed() {
                break;
            }
        }
    });
    tokio::spawn(subscribe_loop(paths.clone(), tx.clone()));
    let ticks = tx.clone();
    tokio::spawn(async move {
        // Picks up hand edits to tasks.md.
        while ticks.send(Msg::Tick).is_ok() {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let mut ui = Ui {
        paths,
        tx,
        input: String::new(),
        entries: vec![Entry::Info(HELP.into())],
        tasks: TaskList::default(),
        status: None,
        connected: false,
        pending: 0,
        scroll: 0,
        selected: None,
        tasks_area: Cell::default(),
        chat_area: Cell::default(),
        split: 38,
        dragging: false,
        task_scroll: 0,
        mouse_capture: true,
        quit: false,
    };
    let mut terminal = ratatui::init();
    // Mouse capture for clicks, drags and the wheel; F2 turns it off to select text.
    // Bracketed paste, so a pasted block arrives as text rather than as Enter presses.
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let result = ui.event_loop(&mut terminal, &mut rx).await;
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn subscribe_loop(paths: Paths, tx: mpsc::UnboundedSender<Msg>) {
    loop {
        if let Ok(mut sub) = client::subscribe(&paths).await {
            let _ = tx.send(Msg::Connected(true));
            while let Ok(Some(ev)) = sub.next().await {
                if tx.send(Msg::Daemon(ev)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Msg::Connected(false));
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

impl Ui {
    async fn event_loop(&mut self, terminal: &mut DefaultTerminal, rx: &mut mpsc::UnboundedReceiver<Msg>) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            let Some(msg) = rx.recv().await else { break };
            self.handle(msg);
            // Drain whatever else is queued before redrawing.
            while let Ok(msg) = rx.try_recv() {
                self.handle(msg);
            }
        }
        Ok(())
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Key(TermEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                match key.code {
                    KeyCode::Char('c') | KeyCode::Char('d') if ctrl => self.quit = true,
                    KeyCode::Char('y') if ctrl => self.copy_last_reply(),
                    KeyCode::F(2) => self.set_mouse_capture(!self.mouse_capture),
                    KeyCode::Left if alt => self.resize(-5),
                    KeyCode::Right if alt => self.resize(5),
                    KeyCode::Enter if alt => self.input.push('\n'),
                    KeyCode::Enter if self.input.trim().is_empty() => self.open_selected(),
                    KeyCode::Enter => self.submit(),
                    KeyCode::Esc if self.input.is_empty() => self.selected = None,
                    KeyCode::Esc => self.input.clear(),
                    KeyCode::Backspace => {
                        self.input.pop();
                    }
                    KeyCode::PageUp => self.scroll += 10,
                    KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
                    KeyCode::Char(c) => self.input.push(c),
                    _ => {}
                }
            }
            Msg::Key(TermEvent::Mouse(mouse)) => self.on_mouse(mouse),
            Msg::Key(TermEvent::Paste(text)) => self.input.push_str(&text.replace("\r\n", "\n").replace('\r', "\n")),
            Msg::Key(_) => {}
            Msg::Daemon(ev) => self.on_event(ev),
            Msg::Tasks(list) => self.tasks = list,
            Msg::Connected(up) => {
                self.connected = up;
                if up {
                    self.refresh_tasks();
                } else {
                    self.status = None;
                }
            }
            Msg::RequestDone => {
                self.pending = self.pending.saturating_sub(1);
                self.refresh_tasks();
            }
            Msg::Tick => self.refresh_tasks(),
        }
    }

    fn on_event(&mut self, ev: Event) {
        let entry = match ev {
            Event::CheckIn { text } => Entry::CheckIn(text),
            Event::Note { text } => Entry::Info(format!("note to self: {text}")),
            Event::Proposal { proposal: p, .. } => Entry::Proposal(format!(
                "{}, awaiting approval: /approve {id} or /reject {id}\n{}",
                p.headline(),
                p.content,
                id = p.id
            )),
            Event::Reply { text, .. } | Event::Ok { text } => Entry::Coach(text),
            Event::Question { text, .. } => Entry::Question(text),
            Event::Preview { kind, content, .. } => Entry::Preview { kind, content },
            Event::Changed { summary, .. } => {
                self.refresh_tasks();
                Entry::Change(summary)
            }
            Event::Report { text, path, .. } => Entry::Coach(format!("{text}\n\nSaved to {path}")),
            Event::Error { text, .. } => Entry::Error(text),
            Event::Status { status } => {
                self.status = Some(status);
                return;
            }
            // Tasks come from refresh_tasks; the rest isn't for the conversation.
            Event::Tasks { .. } | Event::TurnDone { .. } | Event::ToolResult { .. } | Event::Done => return,
        };
        self.entries.push(entry);
        self.scroll = 0;
    }

    fn refresh_tasks(&self) {
        let (paths, tx) = (self.paths.clone(), self.tx.clone());
        tokio::spawn(async move {
            let _ = client::request(&paths, Request::Tasks, |ev| {
                if let Event::Tasks { list } = ev {
                    let _ = tx.send(Msg::Tasks(list));
                }
            })
            .await;
        });
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let at = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if self.on_border(at) => self.dragging = true,
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((id, on_mark)) = self.task_at(at) {
                    if on_mark { self.toggle(id) } else { self.select(id) }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                let body = self.tasks_area.get().union(self.chat_area.get());
                if let Some(percent) = ((at.x.saturating_sub(body.x) + 1) * 100).checked_div(body.width) {
                    self.split = percent.clamp(MIN_SPLIT, MAX_SPLIT);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging = false,
            MouseEventKind::ScrollUp if self.chat_area.get().contains(at) => self.scroll += 3,
            MouseEventKind::ScrollDown if self.chat_area.get().contains(at) => {
                self.scroll = self.scroll.saturating_sub(3)
            }
            MouseEventKind::ScrollUp if self.tasks_area.get().contains(at) => {
                self.task_scroll = self.task_scroll.saturating_sub(3)
            }
            MouseEventKind::ScrollDown if self.tasks_area.get().contains(at) => self.task_scroll += 3,
            _ => {}
        }
    }

    /// The task drawn at a screen position, and whether the hit is on its mark.
    fn task_at(&self, at: Position) -> Option<(TaskId, bool)> {
        let inner = self.tasks_area.get().inner(ratatui::layout::Margin::new(1, 1));
        if !inner.contains(at) {
            return None;
        }
        let row = (at.y - inner.y) as usize + self.task_scroll_in(inner);
        let (_, id) = self.task_lines(inner.width as usize).into_iter().nth(row)?;
        Some((id?, at.x - inner.x < 3))
    }

    fn toggle(&mut self, id: TaskId) {
        self.send(Request::Toggle { id });
    }

    /// Whether `at` is on the border between the two panes, where a drag resizes them.
    fn on_border(&self, at: Position) -> bool {
        let tasks = self.tasks_area.get();
        let edge = tasks.right().saturating_sub(1);
        (at.x == edge || at.x == edge + 1) && at.y >= tasks.y && at.y < tasks.bottom()
    }

    fn resize(&mut self, by: i16) {
        self.split = self.split.saturating_add_signed(by).clamp(MIN_SPLIT, MAX_SPLIT);
    }

    /// Select mode: with capture off, the terminal's own drag-to-select and copy work.
    fn set_mouse_capture(&mut self, on: bool) {
        self.mouse_capture = on;
        let _ = if on {
            execute!(std::io::stdout(), EnableMouseCapture)
        } else {
            execute!(std::io::stdout(), DisableMouseCapture)
        };
        let what = if on {
            "Clicks and scrolling are back. F2 to select text again."
        } else {
            "Select mode: drag to select and copy with your terminal. F2 to go back."
        };
        self.entries.push(Entry::Info(what.into()));
        self.scroll = 0;
    }

    /// The coach's last reply, as plain text.
    fn last_reply(&self) -> Option<&str> {
        self.entries.iter().rev().find_map(|e| match e {
            Entry::Coach(t) | Entry::Question(t) | Entry::Proposal(t) | Entry::CheckIn(t) => Some(t.as_str()),
            Entry::Preview { content, .. } => Some(content.as_str()),
            _ => None,
        })
    }

    fn copy_last_reply(&mut self) {
        let Some(text) = self.last_reply().map(String::from) else {
            self.entries.push(Entry::Error("nothing to copy yet".into()));
            return;
        };
        let note = match copy_to_clipboard(&text) {
            Ok(()) => Entry::Info(format!("copied {} characters", text.chars().count())),
            Err(e) => Entry::Error(format!("couldn't copy: {e}")),
        };
        self.entries.push(note);
        self.scroll = 0;
    }

    /// The task pane's scroll, kept so the last row stays in view.
    fn task_scroll_in(&self, inner: Rect) -> usize {
        let rows = self.task_lines(inner.width as usize).len();
        self.task_scroll.min(rows.saturating_sub(inner.height as usize))
    }

    /// Selects a task and shows its details, or clears the selection on a second click.
    fn select(&mut self, id: TaskId) {
        if self.selected.as_ref() == Some(&id) {
            self.selected = None;
            return;
        }
        let Ok(task) = self.tasks.get(id.as_str()) else { return };
        let mut text = format!("{} · {}", task.id, task.title);
        let mut facts = vec![task.state.word().to_string()];
        if let Some(due) = task.due {
            facts.push(format!("due {}", due.format("%a %m/%d")));
        }
        if let Some(by) = task.blocked_by() {
            facts.push(format!("blocked by {by}"));
        }
        text.push_str(&format!("\n{}", facts.join(" · ")));
        for note in &task.notes {
            text.push_str(&format!("\nnote: {note}"));
        }
        for link in &task.links {
            text.push_str(&format!("\nlink: {link}"));
        }
        if let Some(source) = &task.source_ref {
            text.push_str(&format!("\nsource: {source}"));
        }
        text.push_str(if task.links.is_empty() { "\n/note <text> adds a note." } else { "\n/note <text> adds a note; Enter opens the first link." });
        self.entries.push(Entry::Task(text));
        self.scroll = 0;
        self.selected = Some(id);
    }

    fn open_selected(&mut self) {
        let Some(id) = &self.selected else { return };
        let Some(link) = self.tasks.get(id.as_str()).ok().and_then(|t| t.links.first().cloned()) else {
            self.entries.push(Entry::Error(format!("{id} has no links")));
            return;
        };
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        match std::process::Command::new(opener).arg(&link).spawn() {
            Ok(_) => self.entries.push(Entry::Info(format!("opened {link}"))),
            Err(e) => self.entries.push(Entry::Error(format!("couldn't open {link}: {e}"))),
        }
    }

    fn submit(&mut self) {
        let text = std::mem::take(&mut self.input);
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let request = match parse_input(text) {
            Input::Quit => {
                self.quit = true;
                return;
            }
            Input::Help => {
                self.entries.push(Entry::Info(HELP.into()));
                return;
            }
            Input::Unknown(c) => {
                self.entries.push(Entry::Error(format!("unknown command {c}. {HELP}")));
                return;
            }
            Input::Copy => return self.copy_last_reply(),
            Input::Note { id, text: note } => match id.or_else(|| self.selected.clone()) {
                Some(id) => Request::Note { id, text: note },
                None => {
                    self.entries.push(Entry::Error("Which task? Click it first, or name it: /note t3 waiting on legal".into()));
                    return;
                }
            },
            Input::Send(request) => request,
        };
        self.entries.push(Entry::You(text.to_string()));
        self.scroll = 0;
        self.send(request);
    }

    fn send(&mut self, request: Request) {
        self.pending += 1;
        let (paths, tx) = (self.paths.clone(), self.tx.clone());
        tokio::spawn(async move {
            let result = client::request(&paths, request, |ev| {
                // Turn events arrive through the subscription; only direct answers come here.
                if matches!(ev, Event::Ok { .. }) || matches!(ev, Event::Error { turn: None, .. }) {
                    let _ = tx.send(Msg::Daemon(ev));
                }
            })
            .await;
            if let Err(e) = result {
                let _ = tx.send(Msg::Daemon(Event::error(format!("{e:#}"))));
            }
            let _ = tx.send(Msg::RequestDone);
        });
    }

    fn draw(&self, f: &mut Frame) {
        // The input grows with what's typed or pasted, up to MAX_INPUT_LINES, then scrolls.
        let input_width = f.area().width.saturating_sub(2).max(10) as usize;
        let typed = wrap(&format!("> {}", self.input), input_width);
        let input_rows = typed.len().clamp(1, MAX_INPUT_LINES);
        let [header, body, input] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(input_rows as u16 + 2),
        ])
        .areas(f.area());
        let [tasks_area, chat_area] =
            Layout::horizontal([Constraint::Percentage(self.split), Constraint::Percentage(100 - self.split)]).areas(body);

        f.render_widget(Paragraph::new(self.header_line()), header);

        self.tasks_area.set(tasks_area);
        self.chat_area.set(chat_area);
        let inner = tasks_area.inner(ratatui::layout::Margin::new(1, 1));
        let skip = self.task_scroll_in(inner);
        let rows = self.task_lines(inner.width as usize);
        let more_below = rows.len() > skip + inner.height as usize;
        let lines: Vec<Line> = rows.into_iter().skip(skip).map(|(line, _)| line).collect();
        let title = match (skip > 0, more_below) {
            (true, true) => " Tasks ↕ ",
            (true, false) => " Tasks ↑ ",
            (false, true) => " Tasks ↓ ",
            (false, false) => " Tasks ",
        };
        let border = if self.dragging { Style::new().fg(Color::Yellow) } else { Style::new() };
        f.render_widget(Paragraph::new(lines).block(Block::bordered().title(title).border_style(border)), tasks_area);

        let inner_width = chat_area.width.saturating_sub(2) as usize;
        let inner_height = chat_area.height.saturating_sub(2) as usize;
        let lines = self.chat_lines(inner_width.max(10));
        let max_scroll = lines.len().saturating_sub(inner_height);
        let scroll = self.scroll.min(max_scroll);
        let start = lines.len().saturating_sub(inner_height + scroll);
        let visible: Vec<Line> = lines.into_iter().skip(start).take(inner_height).collect();
        let title = if scroll > 0 { format!(" Coach (↑{scroll}) ") } else { " Coach ".into() };
        f.render_widget(Paragraph::new(visible).block(Block::bordered().title(title)), chat_area);

        // Show the end of the input, where the cursor is.
        let shown: Vec<Line> = typed[typed.len() - input_rows..].iter().map(|l| Line::raw(l.clone())).collect();
        let title = if typed.len() > input_rows { format!(" ↑{} more ", typed.len() - input_rows) } else { String::new() };
        f.render_widget(Paragraph::new(shown).block(Block::bordered().title(title)), input);
        let last = typed.last().map_or(0, |l| l.chars().count()) as u16;
        let cursor = Position::new((input.x + 1 + last).min(input.right().saturating_sub(2)), input.y + input_rows as u16);
        f.set_cursor_position(cursor);
    }

    fn header_line(&self) -> Line<'static> {
        let mut spans = vec![Span::styled(" Focus Coach ", Style::new().bold().reversed()), Span::raw(" ")];
        match (&self.status, self.connected) {
            (_, false) => spans.push(Span::styled("connecting to the daemon…", Style::new().dim())),
            (None, true) => {}
            (Some(s), true) => {
                if s.paused {
                    spans.push(Span::styled("⏸ paused until your next message", Style::new().fg(Color::Yellow)));
                } else if let Some(t) = s.next_checkin {
                    spans.push(Span::raw(format!("next check-in {}", t.format("%a %H:%M"))));
                }
                if s.awaiting_reply {
                    spans.push(Span::styled("  ● waiting on your reply", Style::new().fg(Color::Cyan)));
                }
            }
        }
        if self.pending > 0 || self.status.as_ref().is_some_and(|s| s.busy) {
            spans.push(Span::styled("  thinking…", Style::new().dim().italic()));
        }
        if !self.mouse_capture {
            spans.push(Span::styled("  select mode (F2 to leave)", Style::new().fg(Color::Black).bg(Color::Yellow)));
        }
        Line::from(spans)
    }

    /// Each row of the task pane, with the id of the task drawn on it.
    /// Each row of the task pane, `width` columns wide, with the id of the task drawn on
    /// it. Long titles wrap; every row of a task maps to that task for clicks.
    fn task_lines(&self, width: usize) -> Vec<(Line<'static>, Option<TaskId>)> {
        let today = Local::now().date_naive();
        let top = self.tasks.top_open().map(|t| t.id.clone());
        let mut lines = Vec::new();
        for section in &self.tasks.sections {
            let visible: Vec<_> = section
                .tasks
                .iter()
                .filter(|t| t.is_visible(today))
                .collect();
            if visible.is_empty() && !section.on_hold {
                continue;
            }
            if !lines.is_empty() {
                lines.push((Line::raw(""), None));
            }
            let heading = if section.on_hold {
                Span::styled(format!("{} · on hold", section.name), Style::new().dim().italic())
            } else {
                Span::styled(section.name.clone(), Style::new().bold())
            };
            lines.push((Line::from(heading), None));
            if section.on_hold {
                continue;
            }
            for task in visible {
                let is_top = top.as_ref() == Some(&task.id);
                let (mark, style) = match task.state {
                    State::Open if is_top => ("▶", Style::new().fg(Color::Yellow).bold()),
                    State::Open => ("○", Style::new()),
                    State::Done { .. } => ("✓", Style::new().fg(Color::Green).dim()),
                    State::OnHold => ("‖", Style::new().dim()),
                    State::Blocked { .. } => ("!", Style::new().fg(Color::Red)),
                };
                let selected = self.selected.as_ref() == Some(&task.id);
                let title_style = if selected { style.reversed() } else { style };
                let due = task.due.map(|d| format!("  due {}", d.format("%m/%d"))).unwrap_or_default();
                let meta = format!("{due}  {}{}", task.id, if task.notes.is_empty() { "" } else { " ✎" });
                // Room for the title after the 3-column mark; the id goes on the last row.
                let room = width.saturating_sub(3).max(8);
                let chunks = wrap(&task.title, room);
                let last = chunks.len() - 1;
                for (i, chunk) in chunks.into_iter().enumerate() {
                    let lead = if i == 0 { format!(" {mark} ") } else { "   ".into() };
                    let mut spans = vec![Span::styled(lead, style), Span::styled(chunk.clone(), title_style)];
                    if i == last {
                        let fits = chunk.chars().count() + meta.chars().count() <= room;
                        if fits {
                            spans.push(Span::styled(meta.clone(), Style::new().dim()));
                        } else {
                            lines.push((Line::from(spans), Some(task.id.clone())));
                            spans = vec![Span::raw("   "), Span::styled(meta.trim_start().to_string(), Style::new().dim())];
                        }
                    }
                    lines.push((Line::from(spans), Some(task.id.clone())));
                }
            }
        }
        if lines.is_empty() {
            lines.push((Line::styled("No tasks yet. Try /start look at my PRs", Style::new().dim()), None));
        }
        lines
    }

    fn chat_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for entry in &self.entries {
            let (prefix, style, text): (&str, Style, String) = match entry {
                Entry::You(t) => ("you  ", Style::new().fg(Color::Blue), t.clone()),
                Entry::CheckIn(t) => ("⏰   ", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD), t.clone()),
                Entry::Coach(t) => ("     ", Style::new(), t.clone()),
                Entry::Question(t) => ("?    ", Style::new().fg(Color::Magenta), t.clone()),
                Entry::Preview { kind, content } => {
                    ("│    ", Style::new().fg(Color::Gray), format!("{kind} (preview, not sent)\n{content}"))
                }
                Entry::Proposal(t) => ("┃    ", Style::new().fg(Color::Magenta), t.clone()),
                Entry::Change(t) => ("  ·  ", Style::new().dim(), t.clone()),
                Entry::Task(t) => ("▸    ", Style::new().fg(Color::Yellow), t.clone()),
                Entry::Error(t) => ("err  ", Style::new().fg(Color::Red), t.clone()),
                Entry::Info(t) => ("     ", Style::new().dim().italic(), t.clone()),
            };
            for (i, chunk) in wrap(&text, width.saturating_sub(prefix.chars().count()).max(10)).into_iter().enumerate() {
                let keep_gutter = i == 0 || matches!(entry, Entry::Preview { .. } | Entry::Proposal(_));
                let lead = if keep_gutter { prefix.to_string() } else { " ".repeat(prefix.chars().count()) };
                out.push(Line::from(vec![Span::styled(lead, style), Span::styled(chunk, style)]));
            }
            if !matches!(entry, Entry::Change(_)) {
                out.push(Line::raw(""));
            }
        }
        out
    }
}

#[derive(Debug)]
enum Input {
    Quit,
    Help,
    Copy,
    /// `/note [id] text`: without an id, the selected task.
    Note { id: Option<TaskId>, text: String },
    Unknown(String),
    Send(Request),
}

/// Slash commands map to requests; anything else goes to the interpreter as-is.
fn parse_input(text: &str) -> Input {
    let (cmd, arg) = text.split_once(' ').map_or((text, ""), |(c, a)| (c, a.trim()));
    let request = match cmd {
        "/quit" | "/q" | "/exit" => return Input::Quit,
        // Typed alone, the plain words quit too; in a sentence they're a message.
        "exit" | "quit" if arg.is_empty() => return Input::Quit,
        "/help" => return Input::Help,
        "/copy" => return Input::Copy,
        "/note" if !arg.is_empty() => {
            let (first, rest) = arg.split_once(char::is_whitespace).map_or((arg, ""), |(f, r)| (f, r.trim()));
            let looks_like_id = first.trim_start_matches('#').trim_start_matches('t').chars().all(|c| c.is_ascii_digit())
                && first.chars().any(|c| c.is_ascii_digit());
            return match (looks_like_id, rest) {
                (true, "") => Input::Unknown("/note needs some text".into()),
                (true, text) => Input::Note { id: Some(TaskId::parse(first)), text: text.into() },
                (false, _) => Input::Note { id: None, text: arg.into() },
            };
        }
        "/next" => Request::Next,
        "/pause" => Request::Pause,
        "/resume" => Request::Resume,
        "/report" => Request::Report,
        "/add" if !arg.is_empty() => add_request(arg),
        "/approve" if !arg.is_empty() => Request::Approve { id: arg.into() },
        "/reject" if !arg.is_empty() => Request::Reject { id: arg.into() },
        "/start" => Request::Start { text: (!arg.is_empty()).then(|| arg.to_string()) },
        c if c.starts_with('/') => return Input::Unknown(c.to_string()),
        _ => Request::Say { text: text.to_string() },
    };
    Input::Send(request)
}

/// `/add` takes a URL (fetched by the coach) or a title with optional `--due` and
/// `--section` (added as is).
fn add_request(arg: &str) -> Request {
    let (mut title, mut section, mut due) = (Vec::new(), None, None);
    let mut words = arg.split_whitespace();
    while let Some(word) = words.next() {
        match word {
            "--due" | "-d" => due = words.next().map(String::from),
            "--section" | "-s" => section = words.next().map(String::from),
            _ => title.push(word),
        }
    }
    Request::add(&title.join(" "), section, due)
}

/// Narrowest and widest the task pane can be dragged, in percent.
const MIN_SPLIT: u16 = 15;
const MAX_SPLIT: u16 = 80;
/// The input box grows to this many lines, then scrolls.
const MAX_INPUT_LINES: usize = 8;

/// Puts `text` on the clipboard: pbcopy, wl-copy or xclip when there is one, else the
/// terminal's own clipboard escape (OSC 52), which works over SSH too.
fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tools: [(&str, &[&str]); 3] = [("pbcopy", &[]), ("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])];
    for (program, args) in tools {
        let Ok(mut child) = std::process::Command::new(program).args(args).stdin(std::process::Stdio::piped()).spawn()
        else {
            continue;
        };
        child.stdin.take().expect("piped").write_all(text.as_bytes())?;
        if child.wait()?.success() {
            return Ok(());
        }
    }
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    out
}

/// Word-wraps each line of `text` to `width` columns.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut current = String::new();
        for word in line.split(' ') {
            let needed = current.chars().count() + word.chars().count() + usize::from(!current.is_empty());
            if needed > width && !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            let mut word = word.to_string();
            while word.chars().count() > width {
                let head: String = word.chars().take(width).collect();
                word = word.chars().skip(width).collect();
                out.push(head);
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(&word);
        }
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn ui(entries: Vec<Entry>, tasks: &str, status: Option<Status>) -> Ui {
        Ui {
            paths: Paths { home: std::env::temp_dir() },
            tx: mpsc::unbounded_channel().0,
            input: "on it".into(),
            entries,
            tasks: TaskList::parse(tasks),
            status,
            connected: true,
            pending: 0,
            scroll: 0,
            selected: None,
            tasks_area: Cell::default(),
            chat_area: Cell::default(),
            split: 38,
            dragging: false,
            task_scroll: 0,
            mouse_capture: true,
            quit: false,
        }
    }

    fn render(ui: &Ui, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn status(paused: bool) -> Status {
        Status {
            paused,
            schedule: "7,27,47 9-17 * * 1-5".into(),
            next_checkin: None,
            awaiting_reply: true,
            missed: 0,
            busy: false,
            top: None,
            top_id: None,
            open: 2,
            session: None,
            pending: vec![],
        }
    }

    #[test]
    fn slash_commands_map_to_requests() {
        assert!(matches!(parse_input("/next"), Input::Send(Request::Next)));
        assert!(matches!(parse_input("/pause"), Input::Send(Request::Pause)));
        assert!(matches!(parse_input("/add https://x.y"), Input::Send(Request::Add { url }) if url == "https://x.y"));
        assert!(matches!(parse_input("/start"), Input::Send(Request::Start { text: None })));
        assert!(matches!(parse_input("/start look at my PRs"), Input::Send(Request::Start { text: Some(t) }) if t == "look at my PRs"));
        assert!(matches!(parse_input("/add"), Input::Unknown(_)), "/add needs something to add");
        assert!(matches!(parse_input("/add renew passport --due fri -s Personal"),
            Input::Send(Request::New { title, section: Some(s), due: Some(d) }) if title == "renew passport" && s == "Personal" && d == "fri"));
        assert!(matches!(parse_input("/add call mom"), Input::Send(Request::New { title, section: None, due: None }) if title == "call mom"));
        assert!(matches!(parse_input("/approve a2"), Input::Send(Request::Approve { id }) if id == "a2"));
        assert!(matches!(parse_input("/reject a2"), Input::Send(Request::Reject { id }) if id == "a2"));
        assert!(matches!(parse_input("/nope"), Input::Unknown(c) if c == "/nope"));
        assert!(matches!(parse_input("/q"), Input::Quit));
        assert!(matches!(parse_input("yup. trying option b."), Input::Send(Request::Say { text }) if text == "yup. trying option b."));
    }

    #[test]
    fn renders_tasks_conversation_and_status() {
        let tasks = "## Today\n- [ ] Review configs {id=t2}\n- [!] Deploy {id=t3}\n- [x] Old {id=t1 done=2020-01-01}\n\n## Design\n> on hold: waiting\n- [ ] Mockups {id=t4}\n";
        let entries = vec![
            Entry::CheckIn("What are you on? Top of the list: Review configs (t2).".into()),
            Entry::You("on it".into()),
            Entry::Change("done t9: something".into()),
            Entry::Preview { kind: "slack message".into(), content: "Hi there".into() },
        ];
        let screen = render(&ui(entries, tasks, Some(status(true))), 110, 24);
        assert!(screen.contains("⏸ paused until your next message"), "{screen}");
        assert!(screen.contains("● waiting on your reply"), "{screen}");
        assert!(screen.contains("▶ Review configs  t2"), "top item is marked:\n{screen}");
        assert!(screen.contains("! Deploy  t3"), "{screen}");
        assert!(!screen.contains("Old"), "done items from earlier days are hidden:\n{screen}");
        assert!(screen.contains("Design · on hold"), "{screen}");
        assert!(!screen.contains("Mockups"), "held items are hidden:\n{screen}");
        assert!(screen.contains("What are you on? Top of the list: Review configs (t2)."), "{screen}");
        assert!(screen.contains("│    slack message (preview, not sent)"), "{screen}");
        assert!(screen.contains("│    Hi there"), "preview body keeps its gutter:\n{screen}");
        assert!(screen.contains("> on it"), "{screen}");
    }

    #[test]
    fn conversation_sticks_to_the_bottom_and_scrolls() {
        let entries: Vec<Entry> = (0..40).map(|i| Entry::Coach(format!("reply {i}"))).collect();
        let mut ui = ui(entries, "", None);
        let screen = render(&ui, 80, 20);
        assert!(screen.contains("reply 39") && !screen.contains("reply 0 "), "{screen}");
        assert!(screen.contains("No tasks yet"), "{screen}");
        ui.scroll = 20;
        let screen = render(&ui, 80, 20);
        assert!(!screen.contains("reply 39") && screen.contains("Coach (↑20)"), "{screen}");
    }

    #[test]
    fn clicks_hit_the_mark_or_the_title() {
        let tasks = "## Today\n- [ ] Review configs {id=t2}\n  - link: https://x.y/pr/1\n  - note: check staging\n- [!] Deploy {id=t3}\n";
        let mut ui = ui(vec![], tasks, None);
        render(&ui, 100, 20);
        // Row 1 is the pane border, row 2 the heading, then one row per task.
        assert_eq!(ui.task_at(Position::new(2, 3)), Some(("t2".into(), true)), "the mark");
        assert_eq!(ui.task_at(Position::new(8, 3)), Some(("t2".into(), false)), "the title");
        assert_eq!(ui.task_at(Position::new(8, 4)), Some(("t3".into(), false)));
        assert_eq!(ui.task_at(Position::new(8, 2)), None, "headings aren't tasks");
        assert_eq!(ui.task_at(Position::new(80, 3)), None, "the coach pane isn't the list");

        ui.select("t2".into());
        let screen = render(&ui, 100, 20);
        assert!(screen.contains("▸    t2 · Review configs"), "{screen}");
        assert!(screen.contains("note: check staging") && screen.contains("link: https://x.y/pr/1"), "{screen}");
        assert_eq!(ui.selected.as_ref().map(TaskId::as_str), Some("t2"));
        ui.select("t2".into());
        assert_eq!(ui.selected, None, "a second click deselects");
    }

    #[test]
    fn long_titles_wrap_and_every_row_clicks_through_to_its_task() {
        let tasks = "## Today\n- [ ] A very long task title that cannot possibly fit on one narrow line {id=t2}\n- [ ] Short {id=t3}\n";
        let mut ui = ui(vec![], tasks, None);
        ui.split = 30;
        let screen = render(&ui, 80, 20);
        assert!(screen.contains("▶ A very long task") && screen.contains("possibly fit"), "wrapped, not cut off:\n{screen}");
        let rows: Vec<_> = ui.task_lines(22).into_iter().filter_map(|(_, id)| id).collect();
        assert!(rows.iter().filter(|id| **id == "t2").count() > 1, "{rows:?}");
        // Row 3 is the title's second line: it still selects t2.
        assert_eq!(ui.task_at(Position::new(5, 4)).map(|(id, _)| id.as_str().to_string()), Some("t2".into()));
    }

    #[test]
    fn dragging_the_border_resizes_the_panes() {
        let mut ui = ui(vec![], "## Today\n- [ ] Ship {id=t1}\n", None);
        render(&ui, 100, 20);
        let border = ui.tasks_area.get().right() - 1;
        let at = |kind, column| MouseEvent { kind, column, row: 5, modifiers: KeyModifiers::NONE };
        ui.on_mouse(at(MouseEventKind::Down(MouseButton::Left), border));
        assert!(ui.dragging);
        ui.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 59));
        ui.on_mouse(at(MouseEventKind::Up(MouseButton::Left), 59));
        assert_eq!((ui.split, ui.dragging), (60, false));
        ui.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 0));
        ui.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 0));
        assert_eq!(ui.split, 60, "only a drag that starts on the border resizes");
        ui.resize(100);
        assert_eq!(ui.split, MAX_SPLIT, "clamped so neither pane disappears");
    }

    #[test]
    fn the_input_grows_with_long_and_pasted_text() {
        let mut ui = ui(vec![], "", None);
        ui.input = "word ".repeat(60);
        let screen = render(&ui, 60, 30);
        assert!(screen.matches("word word").count() >= 4, "wraps over several lines:\n{screen}");
        ui.handle(Msg::Key(TermEvent::Paste("line one\r\nline two".into())));
        assert!(ui.input.ends_with("line one\nline two"), "a paste is text, not Enter presses: {:?}", ui.input);
        ui.input = "x\n".repeat(30);
        let screen = render(&ui, 60, 30);
        assert!(screen.contains("more"), "past the limit it scrolls and says so:\n{screen}");
    }

    #[test]
    fn notes_copy_and_exit_commands() {
        assert!(matches!(parse_input("/note t3 waiting on legal"), Input::Note { id: Some(id), text } if id == "t3" && text == "waiting on legal"));
        assert!(matches!(parse_input("/note waiting on legal"), Input::Note { id: None, text } if text == "waiting on legal"));
        assert!(matches!(parse_input("/copy"), Input::Copy));
        for quit in ["exit", "/exit", "quit", "/quit", "/q"] {
            assert!(matches!(parse_input(quit), Input::Quit), "{quit}");
        }
        assert!(matches!(parse_input("exit criteria for launch?"), Input::Send(Request::Say { .. })), "a sentence is a message");

        let mut ui = ui(vec![Entry::Coach("first".into()), Entry::Change("· done t1".into())], "", None);
        assert_eq!(ui.last_reply(), Some("first"), "changes aren't replies");
        ui.entries.push(Entry::Coach("second".into()));
        assert_eq!(ui.last_reply(), Some("second"));
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn wraps_words_and_long_tokens() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }
}

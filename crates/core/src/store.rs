//! The task list, kept as a markdown file the person can edit by hand.
//!
//! ```text
//! ## Release work
//! > on hold: waiting on design
//!
//! - [ ] Merge and redeploy lodash {id=t3 p=1 due=2026-10-30}
//!   - link: https://github.com/org/repo/pull/12
//!   - note: needs a second reviewer
//! - [x] PRs cut for lodash {id=t2 done=2026-10-21}
//! ```
//!
//! Markers: `[ ]` open, `[x]` done, `[-]` on hold, `[!]` blocked. Items written by hand
//! without an `{id=..}` get one on the next save. The file is re-read before every
//! change, so hand edits are always the current state.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveDate};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Open,
    Done,
    OnHold,
    Blocked,
}

impl State {
    fn marker(self) -> char {
        match self {
            State::Open => ' ',
            State::Done => 'x',
            State::OnHold => '-',
            State::Blocked => '!',
        }
    }

    fn from_marker(c: char) -> Option<Self> {
        match c {
            ' ' => Some(State::Open),
            'x' | 'X' => Some(State::Done),
            '-' | '~' => Some(State::OnHold),
            '!' => Some(State::Blocked),
            _ => None,
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().replace([' ', '-'], "_").as_str() {
            "open" | "reopen" | "reopened" | "moving" => State::Open,
            "done" => State::Done,
            "on_hold" | "hold" | "held" => State::OnHold,
            "blocked" => State::Blocked,
            other => bail!("unknown state {other:?}; use open, on_hold, blocked or done"),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub state: State,
    pub links: Vec<String>,
    pub priority: Option<u8>,
    pub due: Option<NaiveDate>,
    pub done_on: Option<NaiveDate>,
    pub blocked_by: Option<String>,
    pub note: Option<String>,
    pub source_ref: Option<String>,
    /// Indented lines under the item that aren't fields; kept verbatim.
    #[serde(skip)]
    pub extra: Vec<String>,
}

impl Task {
    pub fn new(id: String, title: String) -> Self {
        Self {
            id,
            title,
            state: State::Open,
            links: Vec::new(),
            priority: None,
            due: None,
            done_on: None,
            blocked_by: None,
            note: None,
            source_ref: None,
            extra: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub name: String,
    pub on_hold: bool,
    pub hold_reason: Option<String>,
    pub tasks: Vec<Task>,
    /// Prose between the heading and the items; kept verbatim.
    #[serde(skip)]
    pub extra: Vec<String>,
}

impl Section {
    fn new(name: String) -> Self {
        Self { name, on_hold: false, hold_reason: None, tasks: Vec::new(), extra: Vec::new() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskList {
    pub sections: Vec<Section>,
}

impl TaskList {
    pub fn parse(text: &str) -> Self {
        let mut list = TaskList::default();
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(name) = line.strip_prefix("## ") {
                list.sections.push(Section::new(name.trim().to_string()));
                continue;
            }
            if trimmed.is_empty() || line.starts_with("# ") || trimmed.starts_with("<!--") {
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix('>') {
                let rest = rest.trim();
                if let Some(reason) = strip_prefix_ci(rest, "on hold") {
                    let section = list.current_section();
                    section.on_hold = true;
                    let reason = reason.trim_start_matches(':').trim();
                    section.hold_reason = (!reason.is_empty()).then(|| reason.to_string());
                    continue;
                }
            }
            let indented = line.starts_with(' ') || line.starts_with('\t');
            if !indented && let Some(task) = parse_task_line(trimmed) {
                list.current_section().tasks.push(task);
                continue;
            }
            if indented && let Some(task) = list.sections.last_mut().and_then(|s| s.tasks.last_mut()) {
                if !parse_field(task, trimmed) {
                    task.extra.push(line.trim_end().to_string());
                }
                continue;
            }
            list.current_section().extra.push(line.trim_end().to_string());
        }
        list.assign_missing_ids();
        list
    }

    fn current_section(&mut self) -> &mut Section {
        if self.sections.is_empty() {
            self.sections.push(Section::new("Inbox".into()));
        }
        self.sections.last_mut().unwrap()
    }

    fn assign_missing_ids(&mut self) {
        let mut next = self.max_id() + 1;
        for task in self.sections.iter_mut().flat_map(|s| s.tasks.iter_mut()) {
            if task.id.is_empty() {
                task.id = format!("t{next}");
                next += 1;
            }
        }
    }

    fn max_id(&self) -> u32 {
        self.tasks()
            .filter_map(|(_, t)| t.id.strip_prefix('t')?.parse().ok())
            .max()
            .unwrap_or(0)
    }

    pub fn next_id(&self) -> String {
        format!("t{}", self.max_id() + 1)
    }

    pub fn tasks(&self) -> impl Iterator<Item = (&Section, &Task)> {
        self.sections.iter().flat_map(|s| s.tasks.iter().map(move |t| (s, t)))
    }

    pub fn render(&self) -> String {
        let mut out = String::from("# Focus tasks\n\n");
        out.push_str(
            "<!-- Edit freely. [ ] open, [x] done, [-] on hold, [!] blocked. \
             Keep {id=..} so the coach can find items; new items get one on save. -->\n",
        );
        for section in &self.sections {
            out.push_str(&format!("\n## {}\n", section.name));
            if section.on_hold {
                match &section.hold_reason {
                    Some(r) => out.push_str(&format!("> on hold: {r}\n")),
                    None => out.push_str("> on hold\n"),
                }
            }
            for line in &section.extra {
                out.push_str(line);
                out.push('\n');
            }
            if !section.tasks.is_empty() {
                out.push('\n');
            }
            for task in &section.tasks {
                render_task(&mut out, task);
            }
        }
        out
    }

    fn locate(&self, id: &str) -> Option<(usize, usize)> {
        let id = normalize_id(id);
        self.sections.iter().enumerate().find_map(|(si, s)| {
            s.tasks.iter().position(|t| t.id == id).map(|ti| (si, ti))
        })
    }

    pub fn get(&self, id: &str) -> Result<&Task> {
        let (si, ti) = self.locate(id).with_context(|| format!("no task {id}"))?;
        Ok(&self.sections[si].tasks[ti])
    }

    pub fn get_mut(&mut self, id: &str) -> Result<&mut Task> {
        let (si, ti) = self.locate(id).with_context(|| format!("no task {id}"))?;
        Ok(&mut self.sections[si].tasks[ti])
    }

    pub fn section_mut(&mut self, name: &str) -> Option<&mut Section> {
        self.sections.iter_mut().find(|s| s.name.eq_ignore_ascii_case(name.trim()))
    }

    fn section_or_create(&mut self, name: &str) -> &mut Section {
        let idx = match self.sections.iter().position(|s| s.name.eq_ignore_ascii_case(name.trim())) {
            Some(i) => i,
            None => {
                self.sections.push(Section::new(name.trim().to_string()));
                self.sections.len() - 1
            }
        };
        &mut self.sections[idx]
    }

    pub fn add(&mut self, section: &str, task: Task) -> String {
        let id = task.id.clone();
        self.section_or_create(section).tasks.push(task);
        id
    }

    pub fn remove(&mut self, id: &str) -> Result<Task> {
        let (si, ti) = self.locate(id).with_context(|| format!("no task {id}"))?;
        Ok(self.sections[si].tasks.remove(ti))
    }

    pub fn move_to(&mut self, id: &str, section: &str) -> Result<()> {
        let task = self.remove(id)?;
        self.section_or_create(section).tasks.push(task);
        Ok(())
    }

    /// Marks `id` done as `done_part` and inserts `remaining_part` right after it.
    pub fn split(&mut self, id: &str, done_part: &str, remaining_part: &str) -> Result<String> {
        let new_id = self.next_id();
        let (si, ti) = self.locate(id).with_context(|| format!("no task {id}"))?;
        let original = &mut self.sections[si].tasks[ti];
        let mut rest = original.clone();
        original.title = done_part.to_string();
        original.state = State::Done;
        original.done_on = Some(Local::now().date_naive());
        rest.id = new_id.clone();
        rest.title = remaining_part.to_string();
        rest.state = State::Open;
        rest.done_on = None;
        self.sections[si].tasks.insert(ti + 1, rest);
        Ok(new_id)
    }

    /// The single item a check-in should name: open, not in a held section,
    /// lowest priority number first, then list order.
    pub fn top_open(&self) -> Option<&Task> {
        self.sections
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.on_hold)
            .flat_map(|(si, s)| s.tasks.iter().enumerate().map(move |(ti, t)| (si, ti, t)))
            .filter(|(_, _, t)| t.state == State::Open)
            .min_by_key(|(si, ti, t)| (t.priority.unwrap_or(u8::MAX), *si, *ti))
            .map(|(_, _, t)| t)
    }
}

fn normalize_id(id: &str) -> String {
    let id = id.trim().trim_start_matches('#');
    if id.chars().all(|c| c.is_ascii_digit()) { format!("t{id}") } else { id.to_string() }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

fn parse_task_line(line: &str) -> Option<Task> {
    let rest = line.strip_prefix("- [").or_else(|| line.strip_prefix("* ["))?;
    let mut chars = rest.chars();
    let state = State::from_marker(chars.next()?)?;
    let rest = chars.as_str().strip_prefix(']')?.trim();

    let (title, meta) = match rest.rfind(" {") {
        Some(i) if rest.ends_with('}') => (&rest[..i], Some(&rest[i + 2..rest.len() - 1])),
        _ => (rest, None),
    };
    let mut task = Task::new(String::new(), title.trim().to_string());
    task.state = state;
    for pair in meta.unwrap_or("").split_whitespace() {
        let Some((key, value)) = pair.split_once('=') else { continue };
        match key {
            "id" => task.id = value.to_string(),
            "p" => task.priority = value.parse().ok(),
            "due" => task.due = value.parse().ok(),
            "done" => task.done_on = value.parse().ok(),
            _ => {}
        }
    }
    Some(task)
}

fn parse_field(task: &mut Task, line: &str) -> bool {
    let Some(body) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) else {
        return false;
    };
    let Some((key, value)) = body.split_once(':') else { return false };
    let value = value.trim().to_string();
    match key.trim() {
        "link" => task.links.push(value),
        "note" => task.note = Some(value),
        "blocked_by" | "blocked by" => task.blocked_by = Some(value),
        "source" => task.source_ref = Some(value),
        _ => return false,
    }
    true
}

fn render_task(out: &mut String, task: &Task) {
    let mut meta = format!("id={}", task.id);
    if let Some(p) = task.priority {
        meta.push_str(&format!(" p={p}"));
    }
    if let Some(d) = task.due {
        meta.push_str(&format!(" due={d}"));
    }
    if let Some(d) = task.done_on {
        meta.push_str(&format!(" done={d}"));
    }
    out.push_str(&format!("- [{}] {} {{{meta}}}\n", task.state.marker(), task.title));
    for link in &task.links {
        out.push_str(&format!("  - link: {link}\n"));
    }
    if let Some(b) = &task.blocked_by {
        out.push_str(&format!("  - blocked_by: {b}\n"));
    }
    if let Some(n) = &task.note {
        out.push_str(&format!("  - note: {n}\n"));
    }
    if let Some(s) = &task.source_ref {
        out.push_str(&format!("  - source: {s}\n"));
    }
    for line in &task.extra {
        out.push_str(line);
        out.push('\n');
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: &Path) -> Self {
        Self { path: path.to_path_buf() }
    }

    pub fn load(&self) -> Result<TaskList> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(TaskList::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TaskList::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    pub fn save(&self, list: &TaskList) -> Result<()> {
        let tmp = self.path.with_extension("md.tmp");
        std::fs::write(&tmp, list.render())?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Re-reads the file, applies `f`, and writes it back.
    pub fn update<R>(&self, f: impl FnOnce(&mut TaskList) -> Result<R>) -> Result<R> {
        let mut list = self.load()?;
        let out = f(&mut list)?;
        self.save(&list)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Focus tasks\n\n## Release\n> on hold: design\n\nSome prose.\n\n- [ ] Merge lodash {id=t3 p=1 due=2026-10-30}\n  - link: https://example.com/pr/1\n  - note: second reviewer\n  random indented line\n- [x] PRs cut {id=t2 done=2026-10-21}\n\n## Today\n- [ ] written by hand\n- [!] stuck {id=t9}\n";

    #[test]
    fn round_trips_and_assigns_ids() {
        let list = TaskList::parse(SAMPLE);
        assert_eq!(list.sections.len(), 2);
        let release = &list.sections[0];
        assert!(release.on_hold);
        assert_eq!(release.hold_reason.as_deref(), Some("design"));
        let t3 = list.get("3").unwrap();
        assert_eq!(t3.priority, Some(1));
        assert_eq!(t3.links, vec!["https://example.com/pr/1"]);
        assert_eq!(t3.extra.len(), 1);
        assert_eq!(list.sections[1].tasks[0].id, "t10");

        let again = TaskList::parse(&list.render());
        assert_eq!(again.render(), list.render());
        assert_eq!(again.sections[0].extra, vec!["Some prose."]);
    }

    #[test]
    fn top_open_skips_held_sections_and_non_open() {
        let list = TaskList::parse(SAMPLE);
        assert_eq!(list.top_open().unwrap().title, "written by hand");
    }

    #[test]
    fn split_marks_done_and_inserts_rest() {
        let mut list = TaskList::parse(SAMPLE);
        let new_id = list.split("t3", "PRs cut", "merge and redeploy").unwrap();
        let s = &list.sections[0];
        assert_eq!(s.tasks[0].state, State::Done);
        assert_eq!(s.tasks[1].id, new_id);
        assert_eq!(s.tasks[1].links.len(), 1);
    }
}

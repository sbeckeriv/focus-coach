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
    /// The highest id already used elsewhere (done.md), so new ids never repeat one.
    #[serde(skip)]
    pub id_floor: u32,
}

impl TaskList {
    pub fn parse(text: &str) -> Self {
        Self::parse_above(text, 0)
    }

    /// Parses, numbering items written without an id above `id_floor`.
    pub fn parse_above(text: &str, id_floor: u32) -> Self {
        let mut list = TaskList { id_floor, ..Default::default() };
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
            .max(self.id_floor)
    }

    pub fn next_id(&self) -> String {
        format!("t{}", self.max_id() + 1)
    }

    pub fn tasks(&self) -> impl Iterator<Item = (&Section, &Task)> {
        self.sections.iter().flat_map(|s| s.tasks.iter().map(move |t| (s, t)))
    }

    pub fn render(&self) -> String {
        self.render_as(
            "# Focus tasks\n\n<!-- Edit freely. [ ] open, [x] done, [-] on hold, [!] blocked. \
             Keep {id=..} so the coach can find items; new items get one on save. -->\n",
        )
    }

    fn render_as(&self, head: &str) -> String {
        let mut out = head.to_string();
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
    /// Removes done tasks finished on or before `cutoff`, with the section each was in.
    /// Done items with no date get `today`, so they leave on schedule too. Sections
    /// that only held archived items go with them.
    pub fn take_done(&mut self, cutoff: NaiveDate, today: NaiveDate) -> Vec<(String, Task)> {
        let mut taken = Vec::new();
        for section in &mut self.sections {
            let (gone, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut section.tasks).into_iter().partition(|t| {
                t.state == State::Done && t.done_on.unwrap_or(today) <= cutoff
            });
            section.tasks = keep;
            for task in &mut section.tasks {
                if task.state == State::Done && task.done_on.is_none() {
                    task.done_on = Some(today);
                }
            }
            taken.extend(gone.into_iter().map(|t| (section.name.clone(), t)));
        }
        let emptied: Vec<&String> = taken.iter().map(|(s, _)| s).collect();
        self.sections.retain(|s| {
            !(emptied.contains(&&s.name) && s.tasks.is_empty() && s.extra.is_empty() && !s.on_hold)
        });
        taken
    }

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

const DONE_HEAD: &str = "# Done\n\n<!-- Finished tasks moved out of tasks.md, newest day first. -->\n";

fn read_list(path: &Path, id_floor: u32) -> Result<TaskList> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(TaskList::parse_above(&text, id_floor)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TaskList { id_floor, ..Default::default() }),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
    /// done.md, where finished tasks go; one section per day.
    archive: Option<PathBuf>,
}

impl Store {
    pub fn new(path: &Path) -> Self {
        Self { path: path.to_path_buf(), archive: None }
    }

    pub fn with_archive(mut self, path: &Path) -> Self {
        self.archive = Some(path.to_path_buf());
        self
    }

    pub fn load(&self) -> Result<TaskList> {
        let floor = match &self.archive {
            Some(path) => read_list(path, 0)?.max_id(),
            None => 0,
        };
        read_list(&self.path, floor)
    }

    pub fn save(&self, list: &TaskList) -> Result<()> {
        write_atomic(&self.path, &list.render())
    }

    /// The archive: one section per day, named YYYY-MM-DD, newest first.
    pub fn load_done(&self) -> Result<TaskList> {
        match &self.archive {
            Some(path) => read_list(path, 0),
            None => Ok(TaskList::default()),
        }
    }

    /// Moves tasks done `after_days` or more days ago to done.md. Returns how many moved.
    pub fn archive_done(&self, today: NaiveDate, after_days: u32) -> Result<usize> {
        let Some(archive) = &self.archive else { return Ok(0) };
        let cutoff = today - chrono::Days::new(after_days.into());
        let mut list = self.load()?;
        let before = list.render();
        let mut taken = list.take_done(cutoff, today);
        if taken.is_empty() {
            // Only rewrite tasks.md when a hand-checked item just got its done date.
            if list.render() != before {
                self.save(&list)?;
            }
            return Ok(0);
        }
        taken.sort_by_key(|(_, t)| t.done_on);
        let mut done = read_list(archive, 0)?;
        let count = taken.len();
        for (from, mut task) in taken {
            task.extra.push(format!("  - from: {from}"));
            let day = task.done_on.unwrap_or(today).to_string();
            match done.sections.iter_mut().find(|s| s.name == day) {
                Some(section) => section.tasks.push(task),
                None => {
                    let mut section = Section::new(day);
                    section.tasks.push(task);
                    done.sections.push(section);
                }
            }
        }
        done.sections.sort_by(|a, b| b.name.cmp(&a.name));
        // Written before tasks.md: a failure in between duplicates an item, never loses one.
        write_atomic(archive, &done.render_as(DONE_HEAD))?;
        self.save(&list)?;
        Ok(count)
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

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    #[test]
    fn archive_moves_old_done_items_to_done_md_by_day() {
        let dir = tempfile::tempdir().unwrap();
        let (tasks, done) = (dir.path().join("tasks.md"), dir.path().join("done.md"));
        std::fs::write(
            &tasks,
            "## Today\n- [x] Old {id=t1 done=2026-10-20}\n  - link: https://x.y/1\n- [x] Today {id=t2 done=2026-10-22}\n- [ ] Open {id=t3}\n\n## Gone\n- [x] Older {id=t4 done=2026-10-19}\n\n## Kept\nSome prose.\n- [x] Hand checked\n",
        )
        .unwrap();
        let store = Store::new(&tasks).with_archive(&done);

        assert_eq!(store.archive_done(day(22), 1).unwrap(), 2);
        let list = store.load().unwrap();
        let ids: Vec<_> = list.tasks().map(|(_, t)| t.id.as_str()).collect();
        assert_eq!(ids, ["t2", "t3", "t5"], "today's done items stay");
        assert!(list.sections.iter().all(|s| s.name != "Gone"), "a section emptied by archiving goes");
        assert_eq!(list.get("t5").unwrap().done_on, Some(day(22)), "hand-checked items get today as their date");

        let text = std::fs::read_to_string(&done).unwrap();
        let newest = text.find("## 2026-10-20").unwrap();
        assert!(newest < text.find("## 2026-10-19").unwrap(), "newest day first:\n{text}");
        assert!(text.contains("- [x] Old {id=t1 done=2026-10-20}\n  - link: https://x.y/1\n  - from: Today"), "{text}");
        assert!(text.contains("- [x] Older {id=t4 done=2026-10-19}\n  - from: Gone"), "{text}");

        // Archived ids are never handed out again.
        std::fs::write(&tasks, "## Today\n- [ ] new by hand\n").unwrap();
        let list = store.load().unwrap();
        assert_eq!(list.tasks().next().unwrap().1.id, "t5");
        assert_eq!(list.next_id(), "t6");

        // Nothing due: tasks.md is left alone.
        let before = std::fs::read_to_string(&tasks).unwrap();
        assert_eq!(store.archive_done(day(22), 1).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&tasks).unwrap(), before);
    }

    #[test]
    fn archive_after_zero_days_moves_items_right_away() {
        let dir = tempfile::tempdir().unwrap();
        let (tasks, done) = (dir.path().join("tasks.md"), dir.path().join("done.md"));
        std::fs::write(&tasks, "## Today\n- [x] Just now {id=t1 done=2026-10-22}\n- [x] Later {id=t2 done=2026-10-21}\n").unwrap();
        let store = Store::new(&tasks).with_archive(&done);
        assert_eq!(store.archive_done(day(22), 0).unwrap(), 2);
        assert_eq!(store.load().unwrap().tasks().count(), 0);
        let more = Store::new(&tasks).with_archive(&done);
        std::fs::write(&tasks, "## Today\n- [x] Again {id=t3 done=2026-10-22}\n").unwrap();
        more.archive_done(day(22), 0).unwrap();
        let text = std::fs::read_to_string(&done).unwrap();
        assert_eq!(text.matches("## 2026-10-22").count(), 1, "same day, same section:\n{text}");
    }

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

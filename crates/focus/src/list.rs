//! `focus tasks`: the list as a terminal reads it, not as the file stores it.

use chrono::NaiveDate;
use focus_core::store::{State, TaskList};

/// ANSI styling, or nothing when stdout isn't a terminal.
struct Paint(bool);

impl Paint {
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.0 { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
    }
    fn bold(&self, t: &str) -> String {
        self.wrap("1", t)
    }
    fn dim(&self, t: &str) -> String {
        self.wrap("2", t)
    }
    fn yellow(&self, t: &str) -> String {
        self.wrap("1;33", t)
    }
    fn red(&self, t: &str) -> String {
        self.wrap("31", t)
    }
    fn green(&self, t: &str) -> String {
        self.wrap("2;32", t)
    }
    fn cyan(&self, t: &str) -> String {
        self.wrap("36", t)
    }
}

/// Open work by section with the active items marked (or, with none, the top item);
/// done items only from today, held sections folded to one line.
pub fn format(list: &TaskList, today: NaiveDate, color: bool) -> String {
    let p = Paint(color);
    // The top item is only worth pointing at when nothing is marked active.
    let top = list.active().is_empty().then(|| list.top_open().map(|t| t.id.clone())).flatten();
    let width = list
        .tasks()
        .filter(|(s, t)| !s.on_hold && t.is_visible(today))
        .map(|(_, t)| t.title.chars().count())
        .max()
        .unwrap_or(0)
        .min(60);

    let mut out = String::new();
    for section in &list.sections {
        let tasks: Vec<_> = section.tasks.iter().filter(|t| t.is_visible(today)).collect();
        if section.on_hold {
            let reason = section.hold_reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
            let count = match section.tasks.len() {
                1 => " (1 item)".to_string(),
                n => format!(" ({n} items)"),
            };
            out.push_str(&p.dim(&format!("{} · on hold{reason}{count}", section.name)));
            out.push_str("\n\n");
            continue;
        }
        if tasks.is_empty() {
            continue;
        }
        out.push_str(&p.bold(&section.name));
        out.push('\n');
        for task in tasks {
            let is_top = top.as_ref() == Some(&task.id);
            let pad = width.saturating_sub(task.title.chars().count());
            let title = format!("{}{}", task.title, " ".repeat(pad));
            let (mark, title) = match task.state {
                State::Open if task.active => (p.cyan("●"), p.cyan(&title)),
                State::Open if is_top => (p.yellow("▶"), p.yellow(&title)),
                State::Open => ("○".to_string(), title),
                State::Done { .. } => (p.green("✓"), p.green(&title)),
                State::OnHold => (p.dim("‖"), p.dim(&title)),
                State::Blocked { .. } => (p.red("!"), title),
            };
            let mut line = format!("  {mark} {title}  {}", p.dim(&format!("{:<4}", task.id)));
            if let Some(due) = task.due.filter(|_| !task.is_done()) {
                let days = (due - today).num_days();
                let when = due.format("%a %m/%d");
                line.push_str(&match days {
                    ..0 => format!("  {}", p.red(&format!("overdue {when}"))),
                    0 => format!("  {}", p.yellow("due today")),
                    _ => format!("  {}", p.dim(&format!("due {when}"))),
                });
            }
            if let Some(by) = task.blocked_by() {
                line.push_str(&format!("  {}", p.red(&format!("blocked: {by}"))));
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out.push('\n');
    }

    let (open, done) = (list.open_count(), list.done_on(today));
    if open == 0 && done == 0 && out.is_empty() {
        return "No tasks yet. Try: focus start \"look at my PRs and make a task list\"\n".into();
    }
    out.push_str(&p.dim(&format!("{open} open · {done} done today")));
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "## Today\n- [ ] Cut lodash PRs {id=t1}\n- [ ] Review billing docs {id=t2 p=1 due=2026-10-24}\n\
                        - [!] Deploy {id=t3}\n  - blocked_by: infra freeze\n- [ ] Renew passport {id=t6 due=2026-10-20}\n\
                        - [x] Old thing {id=t4 done=2026-10-01}\n- [x] Shipped today {id=t5 done=2026-10-22}\n\n\
                        ## Design\n> on hold: waiting on design\n- [ ] Review mockups {id=t7}\n- [ ] Prep message {id=t8}\n";

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 22).unwrap()
    }

    #[test]
    fn reads_like_a_list_not_a_file() {
        let out = format(&TaskList::parse(LIST), today(), false);
        assert_eq!(
            out,
            "Today\n\
             \x20 ○ Cut lodash PRs       t1\n\
             \x20 ▶ Review billing docs  t2    due Sat 10/24\n\
             \x20 ! Deploy               t3    blocked: infra freeze\n\
             \x20 ○ Renew passport       t6    overdue Tue 10/20\n\
             \x20 ✓ Shipped today        t5\n\
             \n\
             Design · on hold: waiting on design (2 items)\n\
             \n\
             3 open · 1 done today\n",
        );
    }

    #[test]
    fn color_only_on_request() {
        let list = TaskList::parse(LIST);
        assert!(!format(&list, today(), false).contains('\x1b'));
        assert!(format(&list, today(), true).contains("\x1b[1;33m▶\x1b[0m"), "the top item is highlighted");
    }

    #[test]
    fn empty_list_says_how_to_start() {
        assert!(format(&TaskList::default(), today(), false).starts_with("No tasks yet"));
    }
}

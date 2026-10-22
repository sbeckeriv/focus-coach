//! What the model may do. List changes apply directly; lookups are read-only;
//! nothing here sends anything outward.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveDate};
use serde_json::{Value, json};

use crate::daemon::App;
use crate::interpreter::TurnCtx;
use crate::lookup;
use crate::protocol::Event;
use crate::provider::ToolCall;
use crate::report;
use crate::store::{State, Task};

pub fn definitions() -> Vec<Value> {
    let task_id = json!({"type": "string", "description": "Task id, e.g. \"t3\""});
    vec![
        tool("check_off", "Mark a task done.", json!({
            "task_id": task_id,
            "note": {"type": "string", "description": "Optional note to keep on the item"},
        }), &["task_id"]),
        tool("add_task", "Add a task to a section (created if missing). Only for work a source or the person says is theirs.", json!({
            "section": {"type": "string"},
            "title": {"type": "string", "description": "Short imperative summary"},
            "links": {"type": "array", "items": {"type": "string"}},
            "due": {"type": "string", "description": "YYYY-MM-DD, only if a source states one"},
            "priority": {"type": "integer", "description": "1 is highest; omit to rank by list order"},
            "note": {"type": "string"},
            "source_ref": {"type": "string", "description": "Where it came from, e.g. a PR URL or \"slack thread\""},
        }), &["section", "title"]),
        tool("edit_task", "Reword or update a task, or move it to another section. Only the given fields change.", json!({
            "task_id": task_id,
            "title": {"type": "string"},
            "section": {"type": "string"},
            "links": {"type": "array", "items": {"type": "string"}, "description": "Replaces the link list"},
            "due": {"type": "string", "description": "YYYY-MM-DD, or \"none\" to clear"},
            "priority": {"type": "integer"},
            "note": {"type": "string"},
        }), &["task_id"]),
        tool("remove_task", "Remove a task that turned out not to be the person's or not real.", json!({
            "task_id": task_id,
            "reason": {"type": "string"},
        }), &["task_id", "reason"]),
        tool("set_state", "Put a task or a whole section on hold, mark a task blocked, or reopen either. Pass a task id or a section name as target.", json!({
            "target": {"type": "string"},
            "state": {"type": "string", "enum": ["open", "on_hold", "blocked"]},
            "reason": {"type": "string", "description": "Why it's held or what it's blocked by"},
        }), &["target", "state"]),
        tool("split_task", "Part of a task is done: the task becomes done_part (checked off) and remaining_part becomes a new open task right after it.", json!({
            "task_id": task_id,
            "done_part": {"type": "string"},
            "remaining_part": {"type": "string"},
            "due": {"type": "string", "description": "YYYY-MM-DD for the remaining part"},
        }), &["task_id", "done_part", "remaining_part"]),
        tool("append_log", "Record one line in the day's log: what the person is on and what changed.", json!({
            "summary": {"type": "string"},
        }), &["summary"]),
        tool("set_schedule", "Replace the one check-in schedule with a 5-field cron expression (minute hour day month weekday), e.g. \"7,27,47 9-17 * * 1-5\" for every 20 minutes during weekday work hours.", json!({
            "cron": {"type": "string"},
        }), &["cron"]),
        tool("pause_checkins", "Stop check-ins until the person's next message.", json!({}), &[]),
        tool("resume_checkins", "Restart check-ins on the current schedule.", json!({}), &[]),
        tool("write_report", "Write the end-of-day report (done by day, next in order, on hold, stale), save it and show it to the person.", json!({}), &[]),
        tool("fetch_url", "Read a link: GitHub PRs and issues through gh, other pages over HTTP. Slack, Notion and Jira links aren't connected yet.", json!({
            "url": {"type": "string"},
        }), &["url"]),
        tool("search_source", "Search a source. github: PRs (or issues with filters.kind=\"issues\") via `gh search`; repos: grep across local repos.", json!({
            "source": {"type": "string", "enum": ["github", "repos", "slack", "notion", "jira"]},
            "query": {"type": "string", "description": "Search text; may be empty when filters say enough"},
            "filters": {"type": "object", "description": "github: author, review_requested, involves, assignee, owner, repo, state (open|closed|merged), kind (prs|issues). Use \"@me\" for the person. repos: path."},
        }), &["source"]),
        tool("pr_status", "Live state of a pull request: open/merged/closed, draft, review decision, mergeability and CI.", json!({
            "pr": {"type": "string", "description": "PR URL, or owner/repo#number"},
        }), &["pr"]),
        tool("read_repo", "Read or grep local code under the configured repos_root to settle a question.", json!({
            "path": {"type": "string", "description": "Relative to repos_root, e.g. \"api/src\" or \".\""},
            "query": {"type": "string", "description": "Text or regex to grep for; omit to read a file or list a directory"},
        }), &["path"]),
        tool("ask_user", "Ask the person one short question when the input is ambiguous, then end your turn.", json!({
            "question": {"type": "string"},
        }), &["question"]),
        tool("preview", "Show the person a draft before anything is sent or saved: a message, commit message, PR description or report.", json!({
            "kind": {"type": "string", "description": "e.g. \"slack message\", \"commit message\""},
            "content": {"type": "string"},
        }), &["kind", "content"]),
    ]
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "input_schema": {"type": "object", "properties": properties, "required": required},
    })
}

/// Runs one call. Failures go back to the model as error results rather than ending the turn.
pub(crate) async fn execute(app: &Arc<App>, ctx: &mut TurnCtx, call: &ToolCall) -> (String, bool) {
    match run(app, ctx, &call.name, &call.input).await {
        Ok(out) => (out, false),
        Err(e) => (format!("error: {e:#}"), true),
    }
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn req<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    str_arg(input, key).with_context(|| format!("missing {key}"))
}

fn date_arg(input: &Value, key: &str) -> Result<Option<NaiveDate>> {
    str_arg(input, key)
        .map(|s| s.parse().with_context(|| format!("{key} must be YYYY-MM-DD, got {s:?}")))
        .transpose()
}

fn links_arg(input: &Value) -> Option<Vec<String>> {
    input.get("links")?.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
}

fn record(app: &App, ctx: &mut TurnCtx, change: String) {
    app.emit(Event::Changed { turn: ctx.turn, summary: change.clone() });
    ctx.changes.push(change);
}

async fn run(app: &Arc<App>, ctx: &mut TurnCtx, name: &str, input: &Value) -> Result<String> {
    let today = Local::now().date_naive();
    match name {
        "check_off" => {
            let id = req(input, "task_id")?;
            let note = str_arg(input, "note");
            let title = app.store.update(|list| {
                let task = list.get_mut(id)?;
                task.state = State::Done;
                task.done_on = Some(today);
                task.blocked_by = None;
                if let Some(note) = note {
                    task.note = Some(note.to_string());
                }
                Ok(task.title.clone())
            })?;
            record(app, ctx, format!("done {id}: {title}"));
            Ok(format!("checked off {id}"))
        }
        "add_task" => {
            let section = req(input, "section")?;
            let title = req(input, "title")?;
            let due = date_arg(input, "due")?;
            let id = app.store.update(|list| {
                let mut task = Task::new(list.next_id(), title.to_string());
                task.links = links_arg(input).unwrap_or_default();
                task.due = due;
                task.priority = input.get("priority").and_then(Value::as_u64).map(|p| p.min(9) as u8);
                task.note = str_arg(input, "note").map(String::from);
                task.source_ref = str_arg(input, "source_ref").map(String::from);
                Ok(list.add(section, task))
            })?;
            record(app, ctx, format!("added {id} to {section}: {title}"));
            Ok(format!("added {id}"))
        }
        "edit_task" => {
            let id = req(input, "task_id")?;
            let due = match str_arg(input, "due") {
                Some("none") => Some(None),
                Some(_) => Some(date_arg(input, "due")?),
                None => None,
            };
            let title = app.store.update(|list| {
                let task = list.get_mut(id)?;
                if let Some(title) = str_arg(input, "title") {
                    task.title = title.to_string();
                }
                if let Some(links) = links_arg(input) {
                    task.links = links;
                }
                if let Some(due) = due {
                    task.due = due;
                }
                if let Some(p) = input.get("priority").and_then(Value::as_u64) {
                    task.priority = Some(p.min(9) as u8);
                }
                if let Some(note) = str_arg(input, "note") {
                    task.note = Some(note.to_string());
                }
                let title = task.title.clone();
                if let Some(section) = str_arg(input, "section") {
                    list.move_to(id, section)?;
                }
                Ok(title)
            })?;
            record(app, ctx, format!("edited {id}: {title}"));
            Ok(format!("updated {id}"))
        }
        "remove_task" => {
            let id = req(input, "task_id")?;
            let reason = req(input, "reason")?;
            let task = app.store.update(|list| list.remove(id))?;
            record(app, ctx, format!("removed {id}: {} ({reason})", task.title));
            Ok(format!("removed {id}"))
        }
        "set_state" => {
            let target = req(input, "target")?;
            let state = State::parse(req(input, "state")?)?;
            let reason = str_arg(input, "reason").map(String::from);
            let change = app.store.update(|list| {
                if let Ok(task) = list.get_mut(target) {
                    task.state = state;
                    match state {
                        State::Blocked => task.blocked_by = reason.clone(),
                        State::OnHold => {
                            if let Some(r) = &reason {
                                task.note = Some(format!("on hold: {r}"));
                            }
                        }
                        State::Open => task.blocked_by = None,
                        State::Done => task.done_on = Some(today),
                    }
                    return Ok(format!("{} {target}: {}", state_word(state), task.title));
                }
                let Some(section) = list.section_mut(target) else {
                    bail!("no task or section called {target:?}");
                };
                match state {
                    State::OnHold => {
                        section.on_hold = true;
                        section.hold_reason = reason.clone();
                    }
                    State::Open => {
                        section.on_hold = false;
                        section.hold_reason = None;
                    }
                    _ => bail!("a section can only be on_hold or open"),
                }
                Ok(format!("{} section {}", state_word(state), section.name))
            })?;
            record(app, ctx, change.clone());
            Ok(change)
        }
        "split_task" => {
            let id = req(input, "task_id")?;
            let done_part = req(input, "done_part")?;
            let rest = req(input, "remaining_part")?;
            let due = date_arg(input, "due")?;
            let new_id = app.store.update(|list| {
                let new_id = list.split(id, done_part, rest)?;
                if due.is_some() {
                    list.get_mut(&new_id)?.due = due;
                }
                Ok(new_id)
            })?;
            record(app, ctx, format!("done {id}: {done_part}; left as {new_id}: {rest}"));
            Ok(format!("{id} checked off; remaining work is {new_id}"))
        }
        "append_log" => {
            ctx.summary = Some(req(input, "summary")?.to_string());
            Ok("logged".into())
        }
        "set_schedule" => {
            let cron = req(input, "cron")?;
            let next = app.set_schedule(cron)?;
            record(app, ctx, format!("schedule set to `{cron}`"));
            Ok(match next {
                Some(t) => format!("schedule replaced; next check-in {}", t.format("%a %Y-%m-%d %H:%M")),
                None => "schedule replaced, but it never fires".into(),
            })
        }
        "pause_checkins" => {
            app.pause();
            record(app, ctx, "check-ins paused until the next message".into());
            Ok("paused until the person's next message".into())
        }
        "resume_checkins" => {
            app.resume_timers();
            record(app, ctx, "check-ins resumed".into());
            Ok("check-ins resumed on the current schedule".into())
        }
        "write_report" => {
            let (text, path) = report::generate(app).await?;
            let path = path.display().to_string();
            app.emit(Event::Report { turn: ctx.turn, text, path: path.clone() });
            Ok(format!("The report is saved to {path} and already shown to the person. Don't repeat it; just say where it's saved."))
        }
        "fetch_url" => lookup::fetch_url(req(input, "url")?).await,
        "search_source" => {
            let filters = input.get("filters").cloned().unwrap_or(Value::Null);
            lookup::search(&app.settings.sources, req(input, "source")?, str_arg(input, "query").unwrap_or(""), &filters).await
        }
        "pr_status" => lookup::pr_status(req(input, "pr")?).await,
        "read_repo" => lookup::read_repo(&app.settings.sources, req(input, "path")?, str_arg(input, "query")).await,
        "ask_user" => {
            let question = req(input, "question")?.to_string();
            app.rt().pending_question = Some(question.clone());
            app.emit(Event::Question { turn: ctx.turn, text: question });
            Ok("Shown to the person. End your turn now with no further text; their answer arrives as the next message.".into())
        }
        "preview" => {
            let kind = req(input, "kind")?.to_string();
            let content = req(input, "content")?.to_string();
            app.emit(Event::Preview { turn: ctx.turn, kind, content });
            Ok("Shown to the person. Nothing was sent or saved.".into())
        }
        other => bail!("unknown tool {other}"),
    }
}

fn state_word(state: State) -> &'static str {
    match state {
        State::Open => "reopened",
        State::Done => "done",
        State::OnHold => "on hold",
        State::Blocked => "blocked",
    }
}

//! What the model may do. List changes apply directly and lookups are read-only;
//! the one outward tool, `comment_on_github`, only runs for an approved action.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::de::{Error as _, IgnoredAny};
use serde::{Deserialize, Deserializer};
use chrono::{Local, NaiveDate};
use serde_json::{Value, json};

use crate::daemon::App;
use crate::interpreter::TurnCtx;
use crate::lookup;
use crate::protocol::Event;
use crate::provider::ToolCall;
use crate::report;
use crate::store::{State, Task, TaskId, parse_due};

pub fn definitions() -> Vec<Value> {
    let task_id = json!({"type": "string", "description": "Task id, e.g. \"t3\""});
    vec![
        tool("check_off", "Mark a task done.", json!({
            "task_id": task_id,
            "note": {"type": "string", "description": "Optional note to add to the item"},
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
            "note": {"type": "string", "description": "Adds a note; earlier notes are kept"},
        }), &["task_id"]),
        tool("set_active", "Mark tasks as what the person is on now, or no longer on. Several can be active at once; a task stays active until turned off.", json!({
            "task_ids": {"type": "array", "items": {"type": "string"}, "description": "Task ids, e.g. [\"t3\", \"t5\"]"},
            "active": {"type": "boolean", "description": "true: on it now; false: stopped or switched away"},
        }), &["task_ids", "active"]),
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
        tool("set_schedule", "Replace the one check-in schedule, until the daemon restarts or the person edits the schedule in focus.toml (which then applies again), with a 5-field cron expression (minute hour day month weekday), e.g. \"7,27,47 9-17 * * 1-5\" for every 20 minutes during weekday work hours.", json!({
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
        tool("propose_action", "Propose something outgoing (a Slack message, a PR or issue comment, an email, a saved draft, a commit) and stop. Nothing happens until the person approves it with /approve <id>; then you get an approved turn to carry it out.", json!({
            "kind": {"type": "string", "description": "e.g. \"slack message\", \"pr comment\", \"email draft\""},
            "target": {"type": "string", "description": "Where it goes: a channel, a PR URL, a person"},
            "content": {"type": "string", "description": "Exactly what will be sent or saved"},
        }), &["kind", "target", "content"]),
        tool("note_to_self", "Send the person a note as a notification and keep it in the log. Only when they ask for a reminder or a note to self; needs no approval.", json!({
            "text": {"type": "string"},
        }), &["text"]),
        tool("comment_on_github", "Post a comment on a GitHub PR or issue through gh. Only works in an approved turn, for the approved action.", json!({
            "url": {"type": "string", "description": "The PR or issue URL"},
            "body": {"type": "string"},
        }), &["url", "body"]),
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
    let parsed = serde_json::from_value::<Call>(json!({"name": call.name, "input": call.input}))
        .with_context(|| format!("bad {} call", call.name));
    let result = match parsed {
        Ok(parsed) => run(app, ctx, parsed).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(out) => (out, false),
        Err(e) => (format!("error: {e:#}"), true),
    }
}

/// Every tool and its arguments, read from the model's call. Field names match the
/// schemas in [`definitions`]; a test checks that each schema still parses into its variant.
#[derive(Debug, Deserialize)]
#[serde(tag = "name", content = "input", rename_all = "snake_case")]
enum Call {
    CheckOff(CheckOff),
    AddTask(AddTask),
    EditTask(EditTask),
    SetActive(SetActive),
    RemoveTask(RemoveTask),
    SetState(SetState),
    SplitTask(SplitTask),
    AppendLog(AppendLog),
    SetSchedule(SetSchedule),
    PauseCheckins(IgnoredAny),
    ResumeCheckins(IgnoredAny),
    WriteReport(IgnoredAny),
    FetchUrl(FetchUrl),
    SearchSource(SearchSource),
    PrStatus(PrStatus),
    ReadRepo(ReadRepo),
    AskUser(AskUser),
    Preview(Preview),
    ProposeAction(ProposeAction),
    NoteToSelf(NoteToSelf),
    CommentOnGithub(CommentOnGithub),
}

#[derive(Debug, Deserialize)]
struct CheckOff {
    #[serde(deserialize_with = "task_id")]
    task_id: TaskId,
    #[serde(default, deserialize_with = "opt_text")]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SetActive {
    #[serde(deserialize_with = "task_ids")]
    task_ids: Vec<TaskId>,
    active: bool,
}

#[derive(Debug, Deserialize)]
struct AddTask {
    #[serde(deserialize_with = "text")]
    section: String,
    #[serde(deserialize_with = "text")]
    title: String,
    #[serde(default, deserialize_with = "opt_links")]
    links: Option<Vec<String>>,
    #[serde(default, deserialize_with = "opt_text")]
    due: Option<String>,
    #[serde(default, deserialize_with = "opt_priority")]
    priority: Option<u8>,
    #[serde(default, deserialize_with = "opt_text")]
    note: Option<String>,
    #[serde(default, deserialize_with = "opt_text")]
    source_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EditTask {
    #[serde(deserialize_with = "task_id")]
    task_id: TaskId,
    #[serde(default, deserialize_with = "opt_text")]
    title: Option<String>,
    #[serde(default, deserialize_with = "opt_text")]
    section: Option<String>,
    /// Replaces the link list.
    #[serde(default, deserialize_with = "opt_links")]
    links: Option<Vec<String>>,
    /// A date, or "none" to clear it.
    #[serde(default, deserialize_with = "opt_text")]
    due: Option<String>,
    #[serde(default, deserialize_with = "opt_priority")]
    priority: Option<u8>,
    #[serde(default, deserialize_with = "opt_text")]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RemoveTask {
    #[serde(deserialize_with = "task_id")]
    task_id: TaskId,
    #[serde(deserialize_with = "text")]
    reason: String,
}

#[derive(Debug, Deserialize)]
struct SetState {
    /// A task id or a section name.
    #[serde(deserialize_with = "text")]
    target: String,
    #[serde(deserialize_with = "text")]
    state: String,
    #[serde(default, deserialize_with = "opt_text")]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SplitTask {
    #[serde(deserialize_with = "task_id")]
    task_id: TaskId,
    #[serde(deserialize_with = "text")]
    done_part: String,
    #[serde(deserialize_with = "text")]
    remaining_part: String,
    #[serde(default, deserialize_with = "opt_text")]
    due: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AppendLog {
    #[serde(deserialize_with = "text")]
    summary: String,
}

#[derive(Debug, Deserialize)]
struct SetSchedule {
    #[serde(deserialize_with = "text")]
    cron: String,
}

#[derive(Debug, Deserialize)]
struct FetchUrl {
    #[serde(deserialize_with = "text")]
    url: String,
}

#[derive(Debug, Deserialize)]
struct SearchSource {
    #[serde(deserialize_with = "text")]
    source: String,
    #[serde(default, deserialize_with = "opt_text")]
    query: Option<String>,
    #[serde(default)]
    filters: Value,
}

#[derive(Debug, Deserialize)]
struct PrStatus {
    #[serde(deserialize_with = "text")]
    pr: String,
}

#[derive(Debug, Deserialize)]
struct ReadRepo {
    #[serde(deserialize_with = "text")]
    path: String,
    #[serde(default, deserialize_with = "opt_text")]
    query: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AskUser {
    #[serde(deserialize_with = "text")]
    question: String,
}

#[derive(Debug, Deserialize)]
struct Preview {
    #[serde(deserialize_with = "text")]
    kind: String,
    #[serde(deserialize_with = "text")]
    content: String,
}

#[derive(Debug, Deserialize)]
struct ProposeAction {
    #[serde(deserialize_with = "text")]
    kind: String,
    #[serde(deserialize_with = "text")]
    target: String,
    #[serde(deserialize_with = "text")]
    content: String,
}

#[derive(Debug, Deserialize)]
struct NoteToSelf {
    #[serde(deserialize_with = "text")]
    text: String,
}

#[derive(Debug, Deserialize)]
struct CommentOnGithub {
    #[serde(deserialize_with = "text")]
    url: String,
    #[serde(deserialize_with = "text")]
    body: String,
}

/// Required text, trimmed; blank counts as missing.
fn text<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s = String::deserialize(d)?;
    match s.trim() {
        "" => Err(D::Error::custom("is empty")),
        t => Ok(t.to_string()),
    }
}

/// Optional text, trimmed; blank, null or absent is None. Models fill unused fields with "".
fn opt_text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
}

/// "t3", "3" or 3 all name task t3.
fn task_id<'de, D: Deserializer<'de>>(d: D) -> Result<TaskId, D::Error> {
    let id = match Value::deserialize(d)? {
        Value::String(s) => s,
        Value::Number(n) => n.to_string(),
        other => return Err(D::Error::custom(format!("expected a task id like \"t3\", got {other}"))),
    };
    match TaskId::parse(&id) {
        id if id.as_str().is_empty() => Err(D::Error::custom("is empty")),
        id => Ok(id),
    }
}

/// One task id or a list of them, in any form `task_id` takes.
fn task_ids<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<TaskId>, D::Error> {
    let items = match Value::deserialize(d)? {
        Value::Array(items) => items,
        one => vec![one],
    };
    items.into_iter().map(|v| task_id(v).map_err(D::Error::custom)).collect()
}

/// 1 is highest; numbers or numeric strings, capped at 9.
fn opt_priority<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u8>, D::Error> {
    let n = match Value::deserialize(d)? {
        Value::Null => return Ok(None),
        Value::Number(n) => n.as_u64(),
        Value::String(s) if s.trim().is_empty() => return Ok(None),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    n.map(|n| Some(n.min(9) as u8)).ok_or_else(|| D::Error::custom("priority must be a whole number, 1 highest"))
}

/// A list of links; a single string is a list of one.
fn opt_links<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Null => None,
        Value::String(s) if s.trim().is_empty() => None,
        Value::String(s) => Some(vec![s.trim().to_string()]),
        Value::Array(items) => Some(items.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).collect()),
        other => return Err(D::Error::custom(format!("links must be a list of URLs, got {other}"))),
    })
}

/// The same forms people type: YYYY-MM-DD, "today", "tomorrow" or a weekday.
fn due(text: Option<&str>) -> Result<Option<NaiveDate>> {
    text.map(|s| parse_due(s, Local::now().date_naive()).context("due")).transpose()
}

fn record(app: &App, ctx: &mut TurnCtx, change: String) {
    app.emit(Event::Changed { turn: Some(ctx.turn), summary: change.clone() });
    ctx.changes.push(change);
}

async fn run(app: &Arc<App>, ctx: &mut TurnCtx, call: Call) -> Result<String> {
    let today = Local::now().date_naive();
    match call {
        Call::CheckOff(CheckOff { task_id: id, note }) => {
            let title = app.store.update(|list| {
                let task = list.get_mut(id.as_str())?;
                task.mark_done(today);
                if let Some(note) = note {
                    task.add_note(note);
                }
                Ok(task.title.clone())
            })?;
            record(app, ctx, format!("done {id}: {title}"));
            Ok(format!("checked off {id}"))
        }
        Call::AddTask(a) => {
            let due = due(a.due.as_deref())?;
            let id = app.store.update(|list| {
                let mut task = Task::new(list.next_id(), a.title.clone());
                task.links = a.links.unwrap_or_default();
                task.due = due;
                task.priority = a.priority;
                task.notes.extend(a.note);
                task.source_ref = a.source_ref;
                Ok(list.add(&a.section, task))
            })?;
            record(app, ctx, format!("added {id} to {}: {}", a.section, a.title));
            Ok(format!("added {id}"))
        }
        Call::EditTask(e) => {
            let id = e.task_id;
            let due = match e.due.as_deref() {
                Some("none") => Some(None),
                other => due(other)?.map(Some),
            };
            let title = app.store.update(|list| {
                let task = list.get_mut(id.as_str())?;
                if let Some(title) = e.title {
                    task.title = title;
                }
                if let Some(links) = e.links {
                    task.links = links;
                }
                if let Some(due) = due {
                    task.due = due;
                }
                if let Some(p) = e.priority {
                    task.priority = Some(p);
                }
                if let Some(note) = e.note {
                    task.add_note(note);
                }
                let title = task.title.clone();
                if let Some(section) = e.section {
                    list.move_to(id.as_str(), &section)?;
                }
                Ok(title)
            })?;
            record(app, ctx, format!("edited {id}: {title}"));
            Ok(format!("updated {id}"))
        }
        Call::RemoveTask(RemoveTask { task_id: id, reason }) => {
            let task = app.store.update(|list| list.remove(id.as_str()))?;
            record(app, ctx, format!("removed {id}: {} ({reason})", task.title));
            Ok(format!("removed {id}"))
        }
        Call::SetActive(SetActive { task_ids, active }) => {
            let summary = app.store.update(|list| list.set_active(&task_ids, active))?;
            record(app, ctx, summary.clone());
            Ok(summary)
        }
        Call::SetState(SetState { target, state, reason }) => {
            let state = State::parse(&state, today, reason.clone())?;
            let change = app.store.update(|list| {
                if let Ok(task) = list.get_mut(&target) {
                    if let (State::OnHold, Some(r)) = (&state, &reason) {
                        task.add_note(format!("on hold: {r}"));
                    }
                    let word = state_word(&state);
                    task.state = state;
                    return Ok(format!("{word} {target}: {}", task.title));
                }
                let Some(section) = list.section_mut(&target) else {
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
                Ok(format!("{} section {}", state_word(&state), section.name))
            })?;
            record(app, ctx, change.clone());
            Ok(change)
        }
        Call::SplitTask(s) => {
            let due = due(s.due.as_deref())?;
            let id = s.task_id;
            let new_id = app.store.update(|list| {
                let new_id = list.split(id.as_str(), &s.done_part, &s.remaining_part)?;
                if due.is_some() {
                    list.get_mut(new_id.as_str())?.due = due;
                }
                Ok(new_id)
            })?;
            record(app, ctx, format!("done {id}: {}; left as {new_id}: {}", s.done_part, s.remaining_part));
            Ok(format!("{id} checked off; remaining work is {new_id}"))
        }
        Call::AppendLog(AppendLog { summary }) => {
            ctx.summary = Some(summary);
            Ok("logged".into())
        }
        Call::SetSchedule(SetSchedule { cron }) => {
            let next = app.set_schedule(&cron)?;
            record(app, ctx, format!("schedule set to `{cron}`"));
            Ok(match next {
                Some(t) => format!("schedule replaced; next check-in {}", t.format("%a %Y-%m-%d %H:%M")),
                None => "schedule replaced, but it never fires".into(),
            })
        }
        Call::PauseCheckins(_) => {
            app.pause();
            record(app, ctx, "check-ins paused until the next message".into());
            Ok("paused until the person's next message".into())
        }
        Call::ResumeCheckins(_) => {
            app.resume_timers();
            record(app, ctx, "check-ins resumed".into());
            Ok("check-ins resumed on the current schedule".into())
        }
        Call::WriteReport(_) => {
            let (text, path) = report::generate(app).await?;
            let path = path.display().to_string();
            app.emit(Event::Report { turn: ctx.turn, text, path: path.clone() });
            Ok(format!("The report is saved to {path} and already shown to the person. Don't repeat it; just say where it's saved."))
        }
        Call::FetchUrl(FetchUrl { url }) => lookup::fetch_url(&url).await,
        Call::SearchSource(s) => {
            lookup::search(&app.config().settings.sources, &s.source, s.query.as_deref().unwrap_or(""), &s.filters).await
        }
        Call::PrStatus(PrStatus { pr }) => lookup::pr_status(&pr).await,
        Call::ReadRepo(r) => lookup::read_repo(&app.config().settings.sources, &r.path, r.query.as_deref()).await,
        Call::AskUser(AskUser { question }) => {
            app.rt().pending_question = Some(question.clone());
            app.emit(Event::Question { turn: ctx.turn, text: question });
            Ok("Shown to the person. End your turn now with no further text; their answer arrives as the next message.".into())
        }
        Call::Preview(Preview { kind, content }) => {
            app.emit(Event::Preview { turn: ctx.turn, kind, content });
            Ok("Shown to the person. Nothing was sent or saved.".into())
        }
        Call::ProposeAction(p) => {
            let proposal = app.propose(&p.kind, &p.target, &p.content);
            let id = proposal.id.clone();
            record(app, ctx, format!("proposed {}", proposal.headline()));
            app.emit(Event::Proposal { turn: ctx.turn, proposal });
            Ok(format!(
                "Proposed as {id}. Nothing was sent. End your turn: say what you proposed and that they can /approve {id} or /reject {id}."
            ))
        }
        Call::NoteToSelf(NoteToSelf { text }) => {
            app.notify(&text).await;
            record(app, ctx, format!("note to self: {text}"));
            Ok("Sent to the person as a notification and logged.".into())
        }
        Call::CommentOnGithub(CommentOnGithub { url, body }) => {
            let Some(approved) = &ctx.approved else {
                bail!("not approved: call propose_action and wait for the person to approve it");
            };
            if approved.target.trim() != url {
                bail!("the approved action {} targets {}, not {url}", approved.id, approved.target);
            }
            let posted = lookup::comment_on_github(&url, &body).await?;
            record(app, ctx, format!("{}: commented on {url}", approved.id));
            Ok(posted)
        }
    }
}

/// What a change to `state` is called in the log: "reopened" rather than "open".
fn state_word(state: &State) -> &'static str {
    match state {
        State::Open => "reopened",
        other => other.word(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call built from a schema's required fields must parse, so the hand-written
    /// schemas and the argument structs can't drift apart.
    #[test]
    fn every_schema_parses_into_its_call() {
        for def in definitions() {
            let name = def["name"].as_str().unwrap();
            let props = &def["input_schema"]["properties"];
            let mut input = serde_json::Map::new();
            for field in def["input_schema"]["required"].as_array().unwrap() {
                let field = field.as_str().unwrap();
                let value = match props[field]["type"].as_str() {
                    Some("array") => json!(["x"]),
                    Some("integer") => json!(1),
                    Some("boolean") => json!(true),
                    Some("object") => json!({}),
                    _ => props[field]["enum"].get(0).cloned().unwrap_or(json!("t1")),
                };
                input.insert(field.to_string(), value);
            }
            let call = json!({"name": name, "input": input});
            if let Err(e) = serde_json::from_value::<Call>(call.clone()) {
                panic!("{name}: {call}: {e}");
            }
        }
    }

    #[test]
    fn arguments_are_read_the_way_models_send_them() {
        let call = |input: Value| serde_json::from_value::<Call>(json!({"name": "add_task", "input": input}));
        let Ok(Call::AddTask(a)) = call(json!({"section": " Today ", "title": "Ship", "priority": "2", "links": "https://x.y", "note": ""})) else {
            panic!()
        };
        assert_eq!((a.section.as_str(), a.priority, a.note), ("Today", Some(2), None));
        assert_eq!(a.links.unwrap(), ["https://x.y"]);
        assert!(call(json!({"section": "Today", "title": "  "})).unwrap_err().to_string().contains("empty"));
        assert!(call(json!({"title": "Ship"})).unwrap_err().to_string().contains("section"));

        let Ok(Call::CheckOff(c)) = serde_json::from_value::<Call>(json!({"name": "check_off", "input": {"task_id": 3}})) else { panic!() };
        assert_eq!(c.task_id, "t3", "a bare number names the task");
        let Ok(Call::SetActive(s)) = serde_json::from_value::<Call>(json!({"name": "set_active", "input": {"task_ids": ["t3", 5], "active": true}})) else { panic!() };
        assert_eq!((s.task_ids, s.active), (vec![TaskId::parse("t3"), TaskId::parse("t5")], true));
        let Ok(Call::SetActive(s)) = serde_json::from_value::<Call>(json!({"name": "set_active", "input": {"task_ids": "t3", "active": false}})) else { panic!() };
        assert_eq!(s.task_ids, vec![TaskId::parse("t3")], "a single id is a list of one");
        assert!(serde_json::from_value::<Call>(json!({"name": "launch_rockets", "input": {}})).is_err());
        assert!(serde_json::from_value::<Call>(json!({"name": "pause_checkins", "input": null})).is_ok());
    }
}

//! Turns free-text input into tool calls against the store and the sources.

use std::sync::Arc;

use anyhow::Result;
use chrono::Local;
use serde_json::json;

use crate::daemon::App;
use crate::log::LogEntry;
use crate::provider::{Provider, Tier};
use crate::tools;

const MAX_STEPS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// Anything the person types: a check-in reply, a command, a link.
    Message,
    /// The first message of the day, naming sources to build the list from.
    Bootstrap,
    /// "Back at it": summarize where things stand.
    Resume,
    /// End-of-day report, run directly by the reporter.
    Report,
}

impl InputKind {
    /// Whether this counts as "the person's next message" for pause-until-next-message.
    pub fn is_from_person(self) -> bool {
        matches!(self, InputKind::Message | InputKind::Bootstrap)
    }
}

/// Per-turn bookkeeping the tools write into.
pub(crate) struct TurnCtx {
    pub turn: u64,
    pub changes: Vec<String>,
    pub summary: Option<String>,
}

const SYSTEM_PROMPT: &str = r#"You are Focus Coach, a local assistant that keeps one person on track during their workday. You keep their task list, read their check-in replies and commands, and do small jobs on list items: lookups, explanations, drafts and readiness checks.

Each message gives you the current time, the check-in state, the task list (a markdown file the person can edit by hand; treat it as the current truth), recent log entries, recent conversation, and the new input. Act through tools; your final text is shown to the person.

How to behave:
1. Keep replies short. A check-in reply usually needs one line back, or nothing beyond the list changes.
2. Items and sections on hold are invisible: don't bring them up until the person says they're moving again.
3. Never invent ownership. Only add a task as the person's if a source says so. If you can't tell, ask.
4. Check facts before adding them. Before listing a PR or ticket as a to-do, check its live status (pr_status / fetch_url): is it merged, closed, already answered? Settle questions from code or config (read_repo) before suggesting the person ask someone.
5. Drift isn't failure. If the person is on something off the list, log it and ask exactly one question: add it to the list, or get back to the top item?
6. Read short replies as status. "yup. trying option b." means log it and move on, with no follow-up questions.
7. A link is a command. A link with "add to the list" means fetch it, summarize it in the title, pick the section, and set a due date if the source states one.
8. "Can we…?" means check, then propose. Look up the facts, say what's known and what's missing, and offer the change. Don't make outward changes without a go-ahead.
9. A pushback is a correction. When the person questions an item ("is that really mine?"), re-check the source, then reword or remove the item if it isn't really theirs.
10. Progress updates ("I cut PRs for lodash", "meeting went well"): check off what's done; use split_task when part of an item is done and part remains.
11. Answer the question asked. For yes/no questions, say yes or no first, then the reason. For design questions, answer, recommend, and say so plainly when an earlier framing (yours included) was wrong.
12. "What's next?" gets the next item and one concrete first step.
13. You can't send messages, commit, open, merge or close anything yet. When asked to write something outgoing, draft it and show it with preview, in the style of the destination (for a commit message, match the repo's recent commits).
14. If a declined or failed action can't be done, don't retry it; offer another route.
15. Use they/them for the person in anything you write about them.
16. When input is ambiguous, call ask_user with one short question and stop.

End every turn that changes the list or reflects real progress with append_log: one line saying what the person is on and what changed."#;

const CLAUDE_CODE_NOTE: &str = "The coach tools above are your mcp__focus__* tools: make every task list, log and schedule change through them, never by editing tasks.md. Claude Code's built-in tools (Bash, Read, Grep, WebFetch...) are there for lookups; rule 13 still holds for anything outgoing.";

/// The binary that serves `mcp <turn>`: whichever front end hosts the daemon,
/// unless FOCUS_MCP_EXE names another (tests run the daemon inside a test binary).
fn mcp_exe() -> Result<std::path::PathBuf> {
    match std::env::var_os("FOCUS_MCP_EXE") {
        Some(exe) => Ok(exe.into()),
        None => Ok(std::env::current_exe()?),
    }
}

fn choose_tier(input: &str, kind: InputKind) -> Tier {
    if kind == InputKind::Bootstrap {
        return Tier::Strong;
    }
    if kind != InputKind::Message {
        return Tier::Light;
    }
    let lower = input.to_lowercase();
    let words = input.split_whitespace().count();
    let design = ["explain", "why ", "how would", "how do we", "can we", "confused", "do we have enough"]
        .iter()
        .any(|k| lower.contains(k));
    if input.len() > 400 || lower.contains("http") || design || (input.contains('?') && words > 14) {
        Tier::Strong
    } else {
        Tier::Light
    }
}

fn context_block(app: &App, kind: InputKind, was_reply: bool) -> Result<String> {
    let list = app.store.load()?;
    let now = Local::now();
    let mut out = format!("Now: {}\n", now.format("%a %Y-%m-%d %H:%M"));
    {
        let mut rt = app.rt();
        if rt.paused {
            out.push_str("Check-ins: paused until the person's next message.\n");
        } else {
            let next = rt.next_checkin.map(|t| t.format("%a %H:%M").to_string()).unwrap_or("none".into());
            out.push_str(&format!("Check-ins: active, schedule `{}`, next {next}.\n", rt.schedule));
        }
        if let Some(hours) = &app.settings.work_hours {
            out.push_str(&format!("Configured work hours: {hours}.\n"));
        }
        if was_reply && let Some(last) = &rt.last_checkin {
            out.push_str(&format!("This input answers the check-in: \"{last}\"\n"));
        }
        if let Some(q) = rt.pending_question.take() {
            out.push_str(&format!("You last asked the person: \"{q}\" (this input may be the answer)\n"));
        }
        if !rt.recap.is_empty() {
            out.push_str("\n<recent_conversation>\n");
            for (input, reply) in &rt.recap {
                out.push_str(&format!("person: {input}\nyou: {reply}\n"));
            }
            out.push_str("</recent_conversation>\n");
        }
    }
    let sources = &app.settings.sources;
    out.push_str(&format!(
        "Sources: github {} (via gh), repos_root {}, Slack/Notion/Jira not connected yet.\n",
        sources.github_org.as_deref().map(|o| format!("org {o}")).unwrap_or("any org".into()),
        sources.repos_root.as_ref().map(|p| p.display().to_string()).unwrap_or("not set".into()),
    ));
    out.push_str(&format!("\n<task_list>\n{}</task_list>\n", list.render()));
    let recent = app.log.recent(10);
    if !recent.is_empty() {
        out.push_str(&format!("\n<recent_log>\n{}\n</recent_log>\n", recent.join("\n\n")));
    }
    if kind == InputKind::Bootstrap {
        out.push_str(
            "\nThis is a bootstrap: fetch from the sources the person names, then build or merge the \
             list (keep existing open items), grouped into sections in priority order. Reply with a \
             short overview and the top item.\n",
        );
    }
    Ok(out)
}

pub(crate) async fn run(app: &Arc<App>, turn: u64, input: &str, kind: InputKind, was_reply: bool) -> Result<String> {
    let label = match (kind, was_reply) {
        (InputKind::Bootstrap, _) => "bootstrap",
        (InputKind::Resume, _) => "resume",
        (_, true) => "check-in reply",
        _ => "message",
    };
    let framed = format!("{}\n<input kind=\"{label}\">\n{input}\n</input>", context_block(app, kind, was_reply)?);
    let mut ctx = TurnCtx { turn, changes: Vec::new(), summary: None };
    let tier = choose_tier(input, kind);

    let reply = if let Provider::ClaudeCode(cc) = &app.provider {
        // Claude Code runs the loop; the coach's tools come back in through `focus mcp`.
        let exe = mcp_exe()?;
        *app.agent_ctx.lock().await = Some(ctx);
        let cwd = app.settings.sources.repos_root.clone().unwrap_or_else(|| app.paths.home.clone());
        let system = format!("{SYSTEM_PROMPT}\n\n{CLAUDE_CODE_NOTE}");
        let mcp = (exe.as_path(), vec!["mcp".to_string(), turn.to_string()], app.paths.home.as_path());
        let session = app.claude_session();
        let result = cc.agent(tier, &system, &framed, mcp, &cwd, (&session.id, session.started)).await;
        if result.is_ok() {
            app.session_started(&session.id);
        }
        ctx = app.agent_ctx.lock().await.take().expect("the turn context is only taken here");
        result?
    } else {
        tool_loop(app, tier, framed, &mut ctx).await?
    };

    let mut entry = LogEntry::new(
        if label == "check-in reply" { "reply" } else { label },
        ctx.summary.clone().unwrap_or_else(|| first_line(&reply)),
    );
    entry.user_reply = (kind != InputKind::Resume).then(|| input.to_string());
    entry.changes = ctx.changes;
    app.log.append(&entry)?;
    Ok(reply)
}

/// The tool loop for providers that hand tool calls back to us.
async fn tool_loop(app: &Arc<App>, tier: Tier, framed: String, ctx: &mut TurnCtx) -> Result<String> {
    let mut messages = vec![json!({"role": "user", "content": framed})];
    let defs = tools::definitions();
    let mut reply = String::new();
    for step in 0..MAX_STEPS {
        let t = app.provider.turn(tier, SYSTEM_PROMPT, &defs, &messages).await?;
        if !t.text.trim().is_empty() {
            reply = t.text.clone();
        }
        messages.push(json!({"role": "assistant", "content": t.content}));
        if t.tool_calls.is_empty() {
            break;
        }
        let mut results = Vec::new();
        for call in &t.tool_calls {
            let (content, is_error) = tools::execute(app, ctx, call).await;
            results.push(json!({
                "type": "tool_result",
                "tool_use_id": call.id,
                "content": content,
                "is_error": is_error,
            }));
        }
        messages.push(json!({"role": "user", "content": results}));
        if step == MAX_STEPS - 1 {
            reply.push_str("\n(stopped after too many steps)");
        }
    }
    Ok(reply)
}

fn first_line(s: &str) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().trim();
    if line.chars().count() > 160 { format!("{}…", line.chars().take(160).collect::<String>()) } else { line.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers() {
        assert_eq!(choose_tier("yup. trying option b.", InputKind::Message), Tier::Light);
        assert_eq!(choose_tier("https://x.y/z add to the list", InputKind::Message), Tier::Strong);
        assert_eq!(choose_tier("can we make the change now?", InputKind::Message), Tier::Strong);
        assert_eq!(choose_tier("look at my PRs", InputKind::Bootstrap), Tier::Strong);
    }
}

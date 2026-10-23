//! Turns free-text input into tool calls against the store and the sources.

use std::sync::Arc;

use anyhow::Result;
use chrono::Local;
use serde_json::json;

use crate::daemon::App;
use crate::log::LogEntry;
use crate::config::LlmSettings;
use crate::protocol::{Proposal, TurnId};
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
    /// The person approved a proposed outgoing action; carry it out.
    Approved,
    /// `progress_check`, run just before a check-in: pick up progress and new work.
    Progress,
}

impl InputKind {
    /// Whether this counts as "the person's next message" for pause-until-next-message.
    pub fn is_from_person(self) -> bool {
        matches!(self, InputKind::Message | InputKind::Bootstrap | InputKind::Approved)
    }
}

/// Per-turn bookkeeping the tools write into.
pub(crate) struct TurnCtx {
    pub turn: TurnId,
    pub changes: Vec<String>,
    pub summary: Option<String>,
    /// Set in an approved turn: the one outgoing action it may carry out.
    pub approved: Option<Proposal>,
}

const SYSTEM_PROMPT: &str = r#"You are Focus Coach, a local assistant that keeps one person on track during their workday. You keep their task list, read their check-in replies and commands, and do small jobs on list items: lookups, explanations, drafts and readiness checks.

Each message gives you the current time, who the person is, the check-in state, the task list (a markdown file the person can edit by hand; treat it as the current truth), recent log entries, recent conversation, and the new input. Act through tools; your final text is shown to the person.

How to behave:
1. Keep replies short. A check-in reply usually needs one line back, or nothing beyond the list changes.
2. Items and sections on hold are invisible: don't bring them up until the person says they're moving again.
3. Never invent ownership. Only add a task as the person's if a source says so. The <about_the_person> block lists their names and handles: match authors, assignees, reviewers and mentions against all of them, and look up their GitHub login with `gh api user --jq .login` when it isn't given and you can. If you still can't tell, ask.
4. Check facts before adding them. Before listing a PR or ticket as a to-do, check its live status (pr_status / fetch_url): is it merged, closed, already answered? Settle questions from code or config (read_repo) before suggesting the person ask someone.
5. Drift isn't failure. When the person says they're working on something that isn't on the list ("started on X", "working on X"), add it and set_active it in the same turn, without asking first; that they're doing it is the source rule 3 needs. If they call it their top priority or urgent, add it with priority 1. Only when it's unclear whether they're on it (a passing mention, "might look at X"), ask exactly one question: add it to the list, or get back to the top item?
5a. Track what they're on. When they say they're working on items ("on the deploy and the review"), set_active those; when they say they've switched away from or stopped one, set_active it off. Several can be active at once, and an item stays active until they say otherwise; don't turn one off just because they mention another. Items marked `active` in the list are what they're on now.
6. Read short replies as status. "yup. trying option b." means log it and move on, with no follow-up questions.
7. A link is a command. A link with "add to the list" means fetch it, summarize it in the title, pick the section, and set a due date if the source states one.
8. "Can we…?" means check, then propose. Look up the facts, say what's known and what's missing, and offer the change. Don't make outward changes without a go-ahead.
9. A pushback is a correction. When the person questions an item ("is that really mine?"), re-check the source, then reword or remove the item if it isn't really theirs.
10. Progress updates ("I cut PRs for lodash", "meeting went well"): check off what's done; use split_task when part of an item is done and part remains.
11. Answer the question asked. For yes/no questions, say yes or no first, then the reason. For design questions, answer, recommend, and say so plainly when an earlier framing (yours included) was wrong.
12. "What's next?" gets the next item and one concrete first step.
13. Nothing outgoing without approval. To send a message, post a comment, save a draft somewhere, commit, open, merge or close anything, call propose_action with the exact content, written in the style of the destination (for a commit message, match the repo's recent commits), and stop. The person approves with /approve <id>. In an approved turn, carry out exactly that action and nothing else, then say what you did. preview shows a draft without proposing it; note_to_self needs no approval when they ask for it.
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

fn choose_tier(input: &str, kind: InputKind, llm: &LlmSettings) -> Tier {
    if matches!(kind, InputKind::Bootstrap | InputKind::Approved) {
        return Tier::Strong;
    }
    if kind != InputKind::Message {
        return Tier::Light;
    }
    let lower = input.to_lowercase();
    let words = input.split_whitespace().count();
    let keyword = llm.strong_keywords.iter().any(|k| lower.contains(&k.to_lowercase()));
    let long_question = input.contains('?') && words > llm.strong_question_words;
    if input.len() >= llm.strong_min_chars || lower.contains("http") || keyword || long_question {
        Tier::Strong
    } else {
        Tier::Light
    }
}

fn context_block(app: &App, kind: InputKind, was_reply: bool, approved: Option<&Proposal>) -> Result<String> {
    let config = app.config();
    let list = app.store.load()?;
    let now = Local::now();
    let mut out = format!("Now: {}\n", now.format("%a %Y-%m-%d %H:%M"));
    {
        let mut rt = app.rt();
        if rt.paused {
            out.push_str("Check-ins: paused until the person's next message.\n");
        } else {
            let next = rt.next_checkin.map(|t| t.format("%a %H:%M").to_string()).unwrap_or("none".into());
            out.push_str(&format!("Check-ins: active, schedule `{}`, next {next}.\n", rt.schedule.as_ref().unwrap_or(&config.settings.schedule)));
        }
        if let Some(hours) = &config.settings.work_hours {
            out.push_str(&format!("Work hours: {hours} (check-ins only fire inside them).\n"));
        }
        if !rt.pending.is_empty() {
            out.push_str("\n<awaiting_approval>\n");
            for p in &rt.pending {
                out.push_str(&format!("{}\n{}\n\n", p.headline(), p.content));
            }
            out.push_str("</awaiting_approval>\n");
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
    if let Some(me) = config.settings.me.describe(config.settings.sources.slack_user_id.as_deref()) {
        out.push_str(&format!("\n<about_the_person>\n{me}</about_the_person>\n"));
    }
    let sources = &config.settings.sources;
    out.push_str(&format!(
        "Sources: github {} (via gh), repos_root {}, Slack/Notion/Jira {}.\n",
        sources.github_org.as_deref().map(|o| format!("org {o}")).unwrap_or("any org".into()),
        sources.repos_root.as_ref().map(|p| p.display().to_string()).unwrap_or("not set".into()),
        if matches!(*config.provider, Provider::ClaudeCode(_)) && config.settings.llm.claude_code_user_mcp {
            "through your MCP tools if you have them (otherwise not connected)"
        } else {
            "not connected yet"
        },
    ));
    out.push_str(&format!("\n<task_list>\n{}</task_list>\n", list.render()));
    let recent = app.log.recent(config.settings.recent_log_entries);
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
    if kind == InputKind::Progress {
        out.push_str(
            "\nThis is a progress check, run just before a check-in while the person may be busy. Look at \
             the sources the input names. Check off a task only when a source clearly shows it's done (a \
             merged PR, a resolved thread); add a task only when a source says it's the person's. When \
             something only might be done or new, don't change the list: mention it instead. Don't ask \
             questions or propose actions. Reply with one short line saying what you changed or noticed, \
             or with nothing at all when there's nothing new; it's shown with the check-in.\n",
        );
    }
    if let Some(p) = approved {
        out.push_str(&format!(
            "\n<approved_action id=\"{}\">\nThe person approved this. Carry it out now, exactly as written, then say what you did.\nkind: {}\ntarget: {}\n{}\n</approved_action>\n",
            p.id, p.kind, p.target, p.content
        ));
    }
    Ok(out)
}

impl InputKind {
    /// How the turn is named in the prompt.
    pub(crate) fn label(self, was_reply: bool) -> &'static str {
        match (self, was_reply) {
            (InputKind::Message, true) => "check-in reply",
            (InputKind::Message, false) => "message",
            (InputKind::Bootstrap, _) => "bootstrap",
            (InputKind::Resume, _) => "resume",
            (InputKind::Approved, _) => "approved",
            (InputKind::Progress, _) => "progress check",
            (InputKind::Report, _) => "report",
        }
    }

    /// The heading of the turn's log entry.
    fn log_kind(self, was_reply: bool) -> &'static str {
        match (self, was_reply) {
            (InputKind::Message, true) => "reply",
            _ => self.label(was_reply),
        }
    }

    /// Whether the input is the person's own words, worth quoting in the log. Resumes
    /// and progress checks are the coach's own prompts.
    fn is_persons_words(self) -> bool {
        !matches!(self, InputKind::Resume | InputKind::Progress)
    }
}

/// Runs one turn. `approved` is the outgoing action an `Approved` turn may carry out.
pub(crate) async fn run(
    app: &Arc<App>,
    turn: TurnId,
    input: &str,
    kind: InputKind,
    was_reply: bool,
    approved: Option<Proposal>,
) -> Result<String> {
    let label = kind.label(was_reply);
    let framed = format!("{}\n<input kind=\"{label}\">\n{input}\n</input>", context_block(app, kind, was_reply, approved.as_ref())?);
    let mut ctx = TurnCtx { turn, changes: Vec::new(), summary: None, approved };
    // One version of the settings for the whole turn, even if focus.toml changes meanwhile.
    let config = app.config();
    let tier = choose_tier(input, kind, &config.settings.llm);

    let reply = if let Provider::ClaudeCode(cc) = &*config.provider {
        // Claude Code runs the loop; the coach's tools come back in through `focus mcp`.
        let exe = mcp_exe()?;
        *app.agent_ctx.lock().await = Some(ctx);
        let cwd = config.settings.sources.repos_root.clone().unwrap_or_else(|| app.paths.home.clone());
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
        tool_loop(app, &config.provider, tier, framed, &mut ctx).await?
    };

    let mut entry = LogEntry::new(
        kind.log_kind(was_reply),
        ctx.summary.clone().unwrap_or_else(|| first_line(&reply)),
    );
    entry.user_reply = kind.is_persons_words().then(|| input.to_string());
    entry.changes = ctx.changes;
    // A progress check that found nothing isn't worth a log entry every 20 minutes.
    let quiet = kind == InputKind::Progress && entry.changes.is_empty() && reply.trim().is_empty();
    if !quiet {
        app.log.append(&entry)?;
    }
    Ok(reply)
}

/// The tool loop for providers that hand tool calls back to us.
async fn tool_loop(app: &Arc<App>, provider: &Provider, tier: Tier, framed: String, ctx: &mut TurnCtx) -> Result<String> {
    let mut messages = vec![json!({"role": "user", "content": framed})];
    let defs = tools::definitions();
    let mut reply = String::new();
    for step in 0..MAX_STEPS {
        let t = provider.turn(tier, SYSTEM_PROMPT, &defs, &messages).await?;
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
        let mut llm = LlmSettings::default();
        assert_eq!(choose_tier("yup. trying option b.", InputKind::Message, &llm), Tier::Light);
        assert_eq!(choose_tier("https://x.y/z add to the list", InputKind::Message, &llm), Tier::Strong);
        assert_eq!(choose_tier("can we make the change now?", InputKind::Message, &llm), Tier::Strong);
        assert_eq!(choose_tier("look at my PRs", InputKind::Bootstrap, &llm), Tier::Strong);
        llm.strong_keywords = vec!["refactor".into()];
        assert_eq!(choose_tier("can we make the change now?", InputKind::Message, &llm), Tier::Light, "keywords come from config");
        assert_eq!(choose_tier("plan the refactor", InputKind::Message, &llm), Tier::Strong);
        llm.strong_min_chars = 10;
        assert_eq!(choose_tier("on it, nearly done", InputKind::Message, &llm), Tier::Strong);
    }
}

//! Drives the built `focus` binary: it starts its own background daemon, which talks
//! to a scripted mock model served from this test process.

#[path = "../../core/tests/support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};

use serde_json::json;
use support::{MockLlm, Reply, calls, text, write_config};

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new(llm: &MockLlm, tasks: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), &llm.url);
        std::fs::write(dir.path().join("tasks.md"), tasks).unwrap();
        Home { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    async fn focus(&self, args: &[&str]) -> (bool, String, String) {
        run(self.dir.path(), args).await
    }
}

/// Stops the background daemon even when an assertion fails.
impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::process::Command::new(env!("CARGO_BIN_EXE_focus"))
            .arg("stop")
            .env("FOCUS_HOME", self.dir.path())
            .output();
    }
}

async fn run(home: &Path, args: &[&str]) -> (bool, String, String) {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_focus"))
        .args(args)
        .env("FOCUS_HOME", home)
        .output()
        .await
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test]
async fn status_and_stop_without_a_daemon_dont_start_one() {
    let llm = MockLlm::start().await;
    let home = Home::new(&llm, "");
    let (ok, out, _) = home.focus(&["status"]).await;
    assert!(ok);
    assert!(out.contains("isn't running"), "{out}");
    let (ok, out, _) = home.focus(&["stop"]).await;
    assert!(ok && out.contains("isn't running"), "{out}");
    assert!(!home.path().join("focus.sock").exists());
}

#[tokio::test]
async fn commands_start_the_daemon_and_print_turns() {
    let llm = MockLlm::start().await;
    let home = Home::new(&llm, "## Today\n- [ ] Cut lodash PRs {id=t1}\n- [ ] Review configs {id=t2}\n");

    let (ok, out, err) = home.focus(&["tasks"]).await;
    assert!(ok, "{err}");
    assert!(out.contains("▶ Cut lodash PRs  t1") && out.contains("2 open · 0 done today"), "{out}");
    assert!(!out.contains('\x1b'), "no color when piped:\n{out}");
    let (_, out, _) = home.focus(&["tasks", "--markdown"]).await;
    assert!(out.contains("- [ ] Cut lodash PRs {id=t1}"), "{out}");

    llm.script([
        calls(&[("check_off", json!({"task_id": "t1"})), ("preview", json!({"kind": "commit message", "content": "Bump lodash"}))]),
        text("Checked off. Next: Review configs (t2)."),
    ]);
    let (ok, out, err) = home.focus(&["say", "I", "cut", "PRs", "for", "lodash"]).await;
    assert!(ok, "{err}");
    assert!(out.contains("  · done t1: Cut lodash PRs"), "{out}");
    assert!(out.contains("┌─ commit message (preview, not sent)\n│ Bump lodash\n└─"), "{out}");
    assert!(out.contains("Checked off. Next: Review configs (t2)."), "{out}");
    assert!(std::fs::read_to_string(home.path().join("tasks.md")).unwrap().contains("- [x] Cut lodash PRs"));
    let request = &llm.requests()[0];
    assert!(support::user_prompt(request).contains("I cut PRs for lodash"), "words are joined into one message");

    let (ok, out, _) = home.focus(&["pause"]).await;
    assert!(ok && out.contains("paused until your next message"), "{out}");
    let (_, out, _) = home.focus(&["status"]).await;
    assert!(out.contains("1 open · top: Review configs (t2) · paused until your next message"), "{out}");

    llm.script([text("Top item: Review configs (t2). First step: open the diff.")]);
    let (ok, out, _) = home.focus(&["next"]).await;
    assert!(ok && out.contains("First step"), "{out}");
    assert!(support::user_prompt(&llm.requests()[2]).contains("what's next?"));

    let (ok, out, _) = home.focus(&["stop"]).await;
    assert!(ok && out.contains("Daemon stopping"), "{out}");
    let (_, out, _) = home.focus(&["status"]).await;
    assert!(out.contains("isn't running"), "{out}");
}

#[tokio::test]
async fn failures_exit_nonzero_with_the_error_on_stderr() {
    let llm = MockLlm::start().await;
    let home = Home::new(&llm, "");

    let (ok, _, err) = home.focus(&["say"]).await;
    assert!(!ok);
    assert!(err.contains("say what?"), "{err}");

    llm.script([Reply::Status(503, "model is loading".into())]);
    let (ok, _, err) = home.focus(&["say", "hello"]).await;
    assert!(!ok, "a failed turn must exit nonzero");
    assert!(err.contains("error:") && err.contains("model is loading"), "{err}");
}

#[tokio::test]
async fn report_prints_and_saves() {
    let llm = MockLlm::start().await;
    let home = Home::new(&llm, "## Today\n- [x] Shipped {id=t1 done=2026-10-21}\n");
    llm.script([text("## Done\n- Shipped (t1)")]);
    let (ok, out, err) = home.focus(&["report"]).await;
    assert!(ok, "{err}");
    assert!(out.contains("## Done\n- Shipped (t1)"), "{out}");
    let saved = out.lines().find_map(|l| l.strip_prefix("Saved to ")).expect("a saved path");
    assert!(std::fs::read_to_string(saved).unwrap().contains("Shipped (t1)"));
}

/// Stands in for `claude -p`: records its flags and prompt, then plays Claude Code's
/// side of MCP against the `focus mcp` server named in --mcp-config.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
dir="$(dirname "$0")"
cfg=""
sid=""
printf '%s\0' "CALL" "$@" >> "$dir/claude-args"
while [ $# -gt 0 ]; do
  case "$1" in --mcp-config) cfg="$2" ;; --session-id) sid="$2" ;; esac
  shift
done
cat > "$dir/claude-stdin"
# Like the real CLI, refuse to create a session id that already exists.
if [ -n "$sid" ] && grep -qx "$sid" "$dir/taken" 2>/dev/null; then
  echo "Error: Session ID $sid is already in use." >&2
  exit 1
fi
if [ -n "$cfg" ]; then
  exe=$(printf '%s' "$cfg" | sed -E 's/.*"command":"([^"]*)".*/\1/')
  turn=$(printf '%s' "$cfg" | sed -E 's/.*"args":\["mcp","([0-9]+)"\].*/\1/')
  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"check_off","arguments":{"task_id":"t1"}}}' \
    '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"check_off","arguments":{"task_id":"t99"}}}' \
    | "$exe" mcp "$turn" > "$dir/mcp-out"
fi
echo '{"type":"result","subtype":"success","is_error":false,"result":"Checked off t1."}'
"#;

/// A FOCUS_HOME whose provider is claude-code, run by FAKE_CLAUDE.
fn claude_code_home(extra_llm: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("claude");
    std::fs::write(&fake, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let config = format!(
        "schedule = \"0 3 1 1 *\"\nnotifier = \"terminal\"\nmorning_bootstrap = \"\"\nreport_at = \"off\"\n\n[llm]\nprovider = \"claude-code\"\ncommand = \"{}\"\nlight_model = \"haiku-test\"\nstrong_model = \"opus-test\"\n{extra_llm}",
        fake.display()
    );
    std::fs::write(dir.path().join("focus.toml"), config).unwrap();
    std::fs::write(dir.path().join("tasks.md"), "## Today\n- [ ] Cut lodash PRs {id=t1}\n").unwrap();
    dir
}

/// The arguments of each `claude` run so far, oldest first.
fn claude_calls(home: &Path) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(home.join("claude-args")).unwrap_or_default();
    text.split("CALL\0").filter(|c| !c.is_empty()).map(|c| c.split_terminator('\0').map(String::from).collect()).collect()
}

fn flag<'a>(call: &'a [String], name: &str) -> Option<&'a str> {
    call.windows(2).find(|w| w[0] == name).map(|w| w[1].as_str())
}

#[tokio::test]
async fn claude_code_provider_runs_turns_through_claude_and_focus_mcp() {
    let dir = claude_code_home("");
    let home = dir.path();
    let read = |name: &str| std::fs::read_to_string(home.join(name)).unwrap_or_default();

    let (ok, out, err) = run(home, &["say", "shipped the lodash PRs"]).await;
    assert!(ok, "{out}{err}");
    assert!(out.contains("Checked off t1."), "the result text is the reply:\n{out}");
    assert!(out.contains("· done t1: Cut lodash PRs"), "tool changes stream like any turn:\n{out}");
    assert!(read("tasks.md").contains("- [x] Cut lodash PRs"), "{}", read("tasks.md"));
    assert!(read("log.md").contains("- done t1: Cut lodash PRs"), "{}", read("log.md"));

    let calls = claude_calls(home);
    let args = &calls[0];
    assert!(args.contains(&"-p".into()) && flag(args, "--output-format") == Some("json"), "{args:?}");
    assert_eq!(flag(args, "--model"), Some("haiku-test"));
    assert!(!args.contains(&"--strict-mcp-config".into()), "your own MCP servers load by default: {args:?}");
    assert!(flag(args, "--allowedTools") == Some("Bash") && args.contains(&"mcp__focus".into()), "all built-ins plus the coach's: {args:?}");
    assert!(args.iter().any(|a| a.starts_with("You are Focus Coach") && a.contains("mcp__focus__")), "{args:?}");
    let prompt = read("claude-stdin");
    assert!(prompt.contains("<input kind=\"message\">") && prompt.contains("shipped the lodash PRs"), "{prompt}");
    assert!(prompt.contains("Cut lodash PRs {id=t1}"), "the prompt carries the task list:\n{prompt}");

    let replies: Vec<serde_json::Value> = read("mcp-out").lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(replies.len(), 4, "one answer per request, none for the notification: {replies:?}");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "focus");
    let tools = replies[1]["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["name"] == "check_off" && t["inputSchema"]["type"] == "object"), "{tools:?}");
    assert_eq!(replies[2]["result"]["content"][0]["text"], "checked off t1");
    assert_eq!(replies[2]["result"]["isError"], false);
    assert_eq!(replies[3]["result"]["isError"], true, "a bad id goes back to the model as a tool error");

    // Plain completions (the report) run with no tools and save no session.
    let (ok, out, err) = run(home, &["report"]).await;
    assert!(ok, "{out}{err}");
    let calls = claude_calls(home);
    let report = calls.last().unwrap();
    assert_eq!(flag(report, "--tools"), Some(""), "{report:?}");
    assert_eq!(flag(report, "--model"), Some("opus-test"));
    assert!(report.contains(&"--no-session-persistence".into()) && flag(report, "--resume").is_none(), "{report:?}");
    let _ = run(home, &["stop"]).await;
}

#[tokio::test]
async fn claude_code_keeps_one_session_per_day_across_restarts() {
    let dir = claude_code_home("");
    let home = dir.path();

    assert!(run(home, &["say", "morning"]).await.0);
    let first = claude_calls(home);
    let id = flag(&first[0], "--session-id").expect("the first turn creates the session").to_string();
    assert!(first[0].iter().all(|a| a != "--no-session-persistence"), "{:?}", first[0]);

    // A restart must not lose the day's session.
    let _ = run(home, &["stop"]).await;
    assert!(run(home, &["say", "on the lodash PRs"]).await.0);
    let calls = claude_calls(home);
    assert_eq!(flag(&calls[1], "--resume"), Some(id.as_str()), "later turns resume it: {:?}", calls[1]);
    assert!(flag(&calls[1], "--session-id").is_none());

    let (_, out, _) = run(home, &["status"]).await;
    assert!(out.contains(&format!("claude --resume {id}")), "{out}");

    // Another day gets a new session.
    let state = std::fs::read_to_string(home.join("state.json")).unwrap();
    let mut state: serde_json::Value = serde_json::from_str(&state).unwrap();
    state["session"]["date"] = json!("2020-01-01");
    let _ = run(home, &["stop"]).await;
    std::fs::write(home.join("state.json"), state.to_string()).unwrap();
    assert!(run(home, &["say", "new day"]).await.0);
    let calls = claude_calls(home);
    let next = flag(&calls[2], "--session-id").expect("a new day creates a new session").to_string();
    assert_ne!(next, id);

    // So do new rules: a session from an older build isn't resumed.
    let _ = run(home, &["stop"]).await;
    let state = std::fs::read_to_string(home.join("state.json")).unwrap();
    let mut state: serde_json::Value = serde_json::from_str(&state).unwrap();
    state["session"]["rules"] = json!(1);
    std::fs::write(home.join("state.json"), state.to_string()).unwrap();
    assert!(run(home, &["say", "new rules"]).await.0);
    let calls = claude_calls(home);
    let newer = flag(&calls[3], "--session-id").expect("new rules create a new session").to_string();
    assert_ne!(newer, next);

    // And so does asking for one.
    let (ok, out, _) = run(home, &["new-session"]).await;
    assert!(ok && out.contains("Started a new session"), "{out}");
    assert!(run(home, &["say", "fresh start"]).await.0);
    let calls = claude_calls(home);
    let fresh = flag(&calls[4], "--session-id").expect("new-session starts a new one");
    assert_ne!(fresh, newer);
    let _ = run(home, &["stop"]).await;
}

#[tokio::test]
async fn a_configured_session_id_is_resumed_even_when_we_didnt_create_it() {
    let id = "8d3f6a2e-5b1c-4f7e-9a0d-2c4b6e8f1a3d";
    let dir = claude_code_home(&format!("session_id = \"{id}\"\n"));
    let home = dir.path();
    // The session already exists in Claude Code, so creating it fails.
    std::fs::write(home.join("taken"), format!("{id}\n")).unwrap();

    let (ok, out, err) = run(home, &["say", "morning"]).await;
    assert!(ok, "{out}{err}");
    let calls = claude_calls(home);
    assert_eq!(flag(&calls[0], "--session-id"), Some(id), "{calls:?}");
    assert_eq!(flag(&calls[1], "--resume"), Some(id), "falls back to resuming it: {calls:?}");

    assert!(run(home, &["say", "next"]).await.0);
    assert_eq!(claude_calls(home).len(), 3, "and resumes directly from then on");
    assert_eq!(flag(&claude_calls(home)[2], "--resume"), Some(id));
    let _ = run(home, &["stop"]).await;
}

#[tokio::test]
async fn claude_code_tools_and_mcp_loading_come_from_config() {
    let dir = claude_code_home("claude_code_tools = [\"Read\", \"mcp__focus\", \"mcp__slack\"]\nclaude_code_user_mcp = false\n");
    let home = dir.path();
    assert!(run(home, &["say", "anything from slack?"]).await.0);
    let args = &claude_calls(home)[0];
    assert!(args.contains(&"--strict-mcp-config".into()), "only the coach's server: {args:?}");
    let allowed: Vec<&str> = args.iter().skip_while(|a| *a != "--allowedTools").skip(1).map(String::as_str).collect();
    assert_eq!(allowed, ["Read", "mcp__focus", "mcp__slack"]);
    let _ = run(home, &["stop"]).await;
}

#[tokio::test]
async fn add_takes_a_title_or_a_link() {
    let llm = MockLlm::start().await;
    let home = Home::new(&llm, "## Today\n- [ ] Existing {id=t1}\n");

    let (ok, out, err) = home.focus(&["add", "renew", "passport", "--due", "2026-11-02", "--section", "Personal"]).await;
    assert!(ok, "{err}");
    assert!(out.contains("· added t2 to Personal: renew passport, due Mon 11/02"), "{out}");
    assert!(llm.requests().is_empty(), "a title is added without the model");

    llm.script([text("Filed it.")]);
    let (ok, out, _) = home.focus(&["add", "https://github.com/o/r/pull/9"]).await;
    assert!(ok && out.contains("Filed it."), "{out}");
    assert!(support::user_prompt(&llm.requests()[0]).contains("https://github.com/o/r/pull/9 add to the list"));

    let (ok, _, err) = home.focus(&["add"]).await;
    assert!(!ok && err.contains("add what?"), "{err}");
}

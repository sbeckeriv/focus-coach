//! End-to-end through the real daemon: socket client → interpreter → local
//! (OpenAI-compatible) provider → tools → tasks.md / log.md. The model is a
//! scripted mock, so these run offline and cost nothing.

mod support;

use std::time::Duration;

use chrono::Local;
use focus_core::store::{State, TaskList};
use focus_core::{Event, Paths, Request, Status, client, daemon};
use serde_json::json;
use support::{MockLlm, Reply, calls, text, tool_results, user_prompt, write_config};
use tempfile::TempDir;
use tokio::task::JoinHandle;

const TASKS: &str = "## Today\nKeep this prose.\n- [ ] Cut lodash PRs {id=t1}\n- [ ] Review billing docs {id=t2}\n\n## Design\n- [ ] Review mockups {id=t3}\n";

struct Harness {
    _dir: TempDir,
    paths: Paths,
    llm: MockLlm,
    daemon: JoinHandle<anyhow::Result<()>>,
}

async fn start(tasks: &str) -> Harness {
    start_with(tasks, |_| String::new()).await
}

/// `extra_config` can set up files in the home and returns TOML appended to focus.toml.
async fn start_with(tasks: &str, extra_config: impl FnOnce(&std::path::Path) -> String) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let llm = MockLlm::start().await;
    write_config(dir.path(), &llm.url);
    let extra = extra_config(dir.path());
    let config = std::fs::read_to_string(dir.path().join("focus.toml")).unwrap() + &extra;
    std::fs::write(dir.path().join("focus.toml"), config).unwrap();
    std::fs::write(dir.path().join("tasks.md"), tasks).unwrap();
    let paths = Paths { home: dir.path().to_path_buf() };
    let daemon = spawn_daemon(&paths).await;
    Harness { _dir: dir, paths, llm, daemon }
}

async fn spawn_daemon(paths: &Paths) -> JoinHandle<anyhow::Result<()>> {
    let handle = tokio::spawn(daemon::run(paths.clone()));
    for _ in 0..100 {
        if client::is_running(paths).await {
            return handle;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon didn't start: {:?}", handle.await);
}

impl Harness {
    async fn send(&self, req: Request) -> Vec<Event> {
        let mut events = Vec::new();
        client::request(&self.paths, req, |ev| events.push(ev)).await.unwrap();
        events
    }

    async fn say(&self, text: &str) -> Vec<Event> {
        self.send(Request::Say { text: text.into() }).await
    }

    async fn status(&self) -> Status {
        match self.send(Request::Status).await.remove(0) {
            Event::Status { status } => status,
            other => panic!("expected status, got {other:?}"),
        }
    }

    async fn tasks(&self) -> TaskList {
        match self.send(Request::Tasks).await.remove(0) {
            Event::Tasks { list } => list,
            other => panic!("expected tasks, got {other:?}"),
        }
    }

    fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.paths.home.join(name)).unwrap_or_default()
    }

    async fn stop(self) -> Paths {
        self.send(Request::Shutdown).await;
        self.daemon.await.unwrap().unwrap();
        self.paths.clone()
    }
}

fn replies(events: &[Event]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Reply { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn changes(events: &[Event]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Changed { summary, .. } => Some(summary.as_str()),
            _ => None,
        })
        .collect()
}

fn errors(events: &[Event]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Error { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn progress_update_checks_off_logs_and_keeps_hand_written_prose() {
    let h = start(TASKS).await;
    h.llm.script([
        calls(&[("check_off", json!({"task_id": "t1"})), ("append_log", json!({"summary": "PRs cut; on t2 next"}))]),
        text("Nice. Next up: Review billing docs (t2)."),
    ]);

    let events = h.say("I cut PRs for lodash").await;

    assert_eq!(replies(&events), ["Nice. Next up: Review billing docs (t2)."]);
    assert_eq!(changes(&events), ["done t1: Cut lodash PRs"]);
    assert!(matches!(events.last(), Some(Event::TurnDone { .. })), "turn must end with TurnDone");

    let today = Local::now().date_naive();
    let tasks = h.file("tasks.md");
    assert!(tasks.contains(&format!("- [x] Cut lodash PRs {{id=t1 done={today}}}")), "{tasks}");
    assert!(tasks.contains("Keep this prose."), "hand-written prose must survive a save:\n{tasks}");

    let log = h.file("log.md");
    assert!(log.contains("> I cut PRs for lodash"), "{log}");
    assert!(log.contains("PRs cut; on t2 next"), "{log}");
    assert!(log.contains("- done t1: Cut lodash PRs"), "{log}");

    let requests = h.llm.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], "mock-light", "short replies use the light model");
    let prompt = user_prompt(&requests[0]);
    assert!(prompt.contains("<task_list>") && prompt.contains("Cut lodash PRs {id=t1}"), "{prompt}");
    assert!(prompt.contains("I cut PRs for lodash"));
    let tool_names: Vec<_> = requests[0]["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].clone()).collect();
    assert!(tool_names.contains(&json!("check_off")) && tool_names.contains(&json!("split_task")));
    assert_eq!(tool_results(&requests[1]), ["checked off t1", "logged"]);
    h.stop().await;
}

#[tokio::test]
async fn split_and_section_hold_change_the_top_item() {
    let h = start(TASKS).await;
    h.llm.script([
        calls(&[
            ("split_task", json!({"task_id": "t1", "done_part": "PRs cut", "remaining_part": "Merge and redeploy", "due": "2026-10-30"})),
            ("set_state", json!({"target": "Today", "state": "on_hold", "reason": "waiting on release"})),
        ]),
        text("Got it."),
    ]);
    h.say("PRs are cut, and we're in a holding pattern for all of today's stuff").await;

    let list = h.tasks().await;
    let today = &list.sections[0];
    assert!(today.on_hold);
    assert_eq!(today.hold_reason.as_deref(), Some("waiting on release"));
    assert_eq!(today.tasks[0].state, State::Done);
    assert_eq!(today.tasks[1].title, "Merge and redeploy");
    assert_eq!(today.tasks[1].due.unwrap().to_string(), "2026-10-30");
    // Held sections are invisible: the top item moves to the next section.
    assert_eq!(h.status().await.top.as_deref(), Some("Review mockups (t3)"));
    h.stop().await;
}

#[tokio::test]
async fn tool_errors_go_back_to_the_model_instead_of_failing_the_turn() {
    let h = start(TASKS).await;
    h.llm.script([
        calls(&[("check_off", json!({"task_id": "t99"})), ("set_state", json!({"target": "t2", "state": "sideways"}))]),
        text("I couldn't find that item."),
    ]);
    let events = h.say("done with the thing").await;

    assert!(errors(&events).is_empty(), "{events:?}");
    assert_eq!(replies(&events), ["I couldn't find that item."]);
    let results = tool_results(&h.llm.requests()[1]);
    assert!(results[0].starts_with("error:") && results[0].contains("no task t99"), "{results:?}");
    assert!(results[1].contains("unknown state"), "{results:?}");
    assert_eq!(h.file("tasks.md"), TASKS, "a failed change must not rewrite the file");
    h.stop().await;
}

#[tokio::test]
async fn ask_user_shows_the_question_and_carries_it_into_the_next_turn() {
    let h = start(TASKS).await;
    h.llm.script([calls(&[("ask_user", json!({"question": "Which PR, lodash or docs?"}))]), text("")]);
    let events = h.say("merged it").await;
    assert!(
        events.iter().any(|e| matches!(e, Event::Question { text, .. } if text == "Which PR, lodash or docs?")),
        "{events:?}"
    );
    assert!(replies(&events).is_empty(), "an empty final text shouldn't produce a reply");

    h.llm.script([text("ok")]);
    h.say("lodash").await;
    let prompt = user_prompt(&h.llm.requests()[2]);
    assert!(prompt.contains("You last asked the person: \"Which PR, lodash or docs?\""), "{prompt}");
    assert!(prompt.contains("person: merged it"), "recent conversation should be included:\n{prompt}");
    h.stop().await;
}

#[tokio::test]
async fn pause_lasts_until_the_next_message_and_survives_a_restart() {
    let h = start(TASKS).await;
    let events = h.send(Request::Pause).await;
    assert!(matches!(&events[0], Event::Ok { text } if text.contains("paused")));
    assert!(h.status().await.paused);
    assert!(h.file("state.json").contains("\"paused\": true"));

    let llm = h.llm.clone();
    let dir = h._dir;
    let paths = Paths { home: dir.path().to_path_buf() };
    client::request(&paths, Request::Shutdown, |_| {}).await.unwrap();
    h.daemon.await.unwrap().unwrap();

    let daemon = spawn_daemon(&paths).await;
    let h = Harness { _dir: dir, paths, llm, daemon };
    assert!(h.status().await.paused, "pause must survive a restart");

    h.llm.script([text("Welcome back.")]);
    h.say("morning").await;
    assert!(!h.status().await.paused, "the next message ends the pause");
    h.stop().await;
}

#[tokio::test]
async fn pause_and_resume_tools_from_free_text() {
    let h = start(TASKS).await;
    h.llm.script([calls(&[("pause_checkins", json!({}))]), text("Paused. Talk tomorrow.")]);
    h.say("stop the check-ins until I message tomorrow").await;
    assert!(h.status().await.paused);

    h.llm.script([calls(&[("resume_checkins", json!({}))]), text("Back on.")]);
    h.say("back at it").await;
    assert!(!h.status().await.paused);
    h.stop().await;
}

#[tokio::test]
async fn toggle_checks_off_and_reopens_without_the_model() {
    let h = start(TASKS).await;
    let events = h.send(Request::Toggle { id: "t2".into() }).await;
    assert!(matches!(&events[0], Event::Changed { summary, .. } if summary == "done t2: Review billing docs"), "{events:?}");
    let today = Local::now().date_naive();
    assert!(h.file("tasks.md").contains(&format!("- [x] Review billing docs {{id=t2 done={today}}}")), "{}", h.file("tasks.md"));
    assert!(h.file("log.md").contains("· checkbox"), "{}", h.file("log.md"));

    h.send(Request::Toggle { id: "t2".into() }).await;
    assert!(h.file("tasks.md").contains("- [ ] Review billing docs {id=t2}"), "{}", h.file("tasks.md"));

    let events = h.send(Request::Toggle { id: "t9".into() }).await;
    assert!(matches!(&events[0], Event::Error { .. }), "{events:?}");
    assert!(h.llm.requests().is_empty(), "toggling never calls the model");
    h.stop().await;
}

#[tokio::test]
async fn hand_edits_are_the_current_state() {
    let h = start(TASKS).await;
    std::fs::write(h.paths.home.join("tasks.md"), "## Inbox\n- [ ] Written by hand\n- [!] Stuck {id=t5}\n").unwrap();

    let list = h.tasks().await;
    let titles: Vec<_> = list.tasks().map(|(_, t)| t.title.as_str()).collect();
    assert_eq!(titles, ["Written by hand", "Stuck"]);
    assert_eq!(list.sections[0].tasks[0].id, "t6", "hand-written items get the next free id");

    h.llm.script([calls(&[("check_off", json!({"task_id": "t6"}))]), text("Done.")]);
    h.say("done with the hand-written one").await;
    assert!(user_prompt(&h.llm.requests()[0]).contains("Written by hand {id=t6}"));
    assert!(h.file("tasks.md").contains("- [x] Written by hand {id=t6"));
    h.stop().await;
}

#[tokio::test]
async fn only_one_daemon_runs_per_home() {
    let h = start(TASKS).await;
    let err = daemon::run(h.paths.clone()).await.unwrap_err();
    assert!(err.to_string().contains("already running"), "{err}");
    h.stop().await;
}

#[tokio::test]
async fn set_schedule_validates_and_replaces_the_one_schedule() {
    let h = start(TASKS).await;
    h.llm.script([
        calls(&[("set_schedule", json!({"cron": "every 20 minutes"}))]),
        calls(&[("set_schedule", json!({"cron": "*/20 9-17 * * 1-5"}))]),
        text("Limited to work hours."),
    ]);
    h.say("limit to work hours").await;

    let results = tool_results(&h.llm.requests()[1]);
    assert!(results[0].starts_with("error:") && results[0].contains("5 fields"), "{results:?}");
    let results = tool_results(&h.llm.requests()[2]);
    assert!(results.last().unwrap().contains("schedule replaced"), "{results:?}");

    let status = h.status().await;
    assert_eq!(status.schedule, "*/20 9-17 * * 1-5");
    let next = status.next_checkin.expect("a work-hours schedule always has a next check-in");
    assert!(next > Local::now());
    assert!(h.file("state.json").contains("*/20 9-17 * * 1-5"));
    h.stop().await;
}

#[tokio::test]
async fn yesterdays_done_items_move_to_done_md_and_reach_the_report() {
    let yesterday = Local::now().date_naive() - chrono::Days::new(1);
    let h = start(&format!("## Today\n- [x] Shipped {{id=t1 done={yesterday}}}\n- [ ] Still open {{id=t2}}\n")).await;

    let list = h.tasks().await;
    assert_eq!(list.tasks().map(|(_, t)| t.id.as_str()).collect::<Vec<_>>(), ["t2"]);
    assert!(h.file("done.md").contains(&format!("## {yesterday}\n\n- [x] Shipped {{id=t1 done={yesterday}}}")), "{}", h.file("done.md"));
    assert!(h.file("log.md").contains("moved 1 done item to done.md"), "{}", h.file("log.md"));

    // A toggle right after doesn't archive today's item (the default keeps it a day).
    h.send(Request::Toggle { id: "t2".into() }).await;
    assert!(h.file("tasks.md").contains("- [x] Still open"));

    h.llm.script([text("## Done\n- t1")]);
    h.send(Request::Report).await;
    let prompt = user_prompt(&h.llm.requests()[0]);
    assert!(prompt.contains("<done_archive") && prompt.contains("Shipped {id=t1"), "{prompt}");
    h.stop().await;
}

#[tokio::test]
async fn report_pauses_writes_the_file_and_uses_the_strong_model() {
    let h = start(TASKS).await;
    h.llm.script([text("## Done\n- t1\n## Next\n- t2\n## On hold\nNone\n## Stale\nNone")]);
    let events = h.send(Request::Report).await;

    let (report, path) = events
        .iter()
        .find_map(|e| match e {
            Event::Report { text, path, .. } => Some((text.clone(), path.clone())),
            _ => None,
        })
        .expect("a report event");
    assert!(report.starts_with("## Done"));
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(path.ends_with(&format!("{}.md", Local::now().date_naive())), "{path}");
    assert!(saved.contains("## Next"));
    assert!(h.status().await.paused, "end of day pauses check-ins");

    let request = &h.llm.requests()[0];
    assert_eq!(request["model"], "mock-strong");
    assert!(request.get("tools").is_none(), "the reporter doesn't get tools");
    assert!(h.file("log.md").contains("report"));
    h.stop().await;
}

#[tokio::test]
async fn write_report_tool_from_free_text() {
    let h = start(TASKS).await;
    h.llm.script([
        calls(&[("pause_checkins", json!({})), ("write_report", json!({}))]),
        text("## Done\nNone"),
        text("Saved the report."),
    ]);
    let events = h.say("I'm done for today. pause the timers, give me a report").await;
    assert!(events.iter().any(|e| matches!(e, Event::Report { .. })), "{events:?}");
    assert!(h.status().await.paused);
    assert_eq!(h.llm.remaining(), 0);
    h.stop().await;
}

#[tokio::test]
async fn provider_failures_surface_as_errors_and_still_end_the_turn() {
    let h = start(TASKS).await;
    h.llm.script([Reply::Status(500, "model not loaded".into())]);
    let events = h.say("what's next?").await;
    let errs = errors(&events);
    assert_eq!(errs.len(), 1, "{events:?}");
    assert!(errs[0].contains("model not loaded"), "{errs:?}");
    assert!(matches!(events.last(), Some(Event::TurnDone { .. })));
    assert!(!h.status().await.busy);
    h.stop().await;
}

#[tokio::test]
async fn bootstrap_uses_the_strong_model_and_resume_summarizes() {
    let h = start("").await;
    h.llm.script([
        calls(&[
            ("add_task", json!({"section": "Reviews", "title": "Review #3813", "links": ["https://github.com/o/r/pull/3813"], "priority": 1})),
            ("add_task", json!({"section": "Today", "title": "Prep message to design"})),
        ]),
        text("Built the list. Top: Review #3813."),
    ]);
    let events = h.send(Request::Start { text: Some("look at my PRs and make a task list".into()) }).await;
    assert_eq!(replies(&events), ["Built the list. Top: Review #3813."]);
    let first = &h.llm.requests()[0];
    assert_eq!(first["model"], "mock-strong");
    assert!(user_prompt(first).contains("This is a bootstrap"));

    let list = h.tasks().await;
    assert_eq!(list.sections.len(), 2);
    assert_eq!(list.sections[0].tasks[0].links, ["https://github.com/o/r/pull/3813"]);
    assert_eq!(list.top_open().unwrap().id, "t1");

    h.llm.script([text("You're on Review #3813.")]);
    h.send(Request::Resume).await;
    let resume = h.llm.requests().pop().unwrap();
    assert_eq!(resume["model"], "mock-light");
    assert!(user_prompt(&resume).contains("<input kind=\"resume\">"));
    h.stop().await;
}

#[tokio::test]
async fn links_and_design_questions_use_the_strong_model() {
    let h = start(TASKS).await;
    h.llm.script([text("ok"), text("Yes: just new routes.")]);
    h.send(Request::Add { url: "https://example.com/spec".into() }).await;
    h.say("I'm still confused, is it just new routes or more work?").await;
    let requests = h.llm.requests();
    assert!(user_prompt(&requests[0]).contains("https://example.com/spec add to the list"));
    assert_eq!(requests[0]["model"], "mock-strong");
    assert_eq!(requests[1]["model"], "mock-strong");
    h.stop().await;
}

#[tokio::test]
async fn subscribers_see_every_clients_turns() {
    let h = start(TASKS).await;
    let mut sub = client::subscribe(&h.paths).await.unwrap();
    assert!(matches!(sub.next().await.unwrap(), Some(Event::Status { .. })), "subscribe starts with a status");

    h.llm.script([calls(&[("preview", json!({"kind": "slack message", "content": "Hi! Can we meet Thursday?"}))]), text("Draft above.")]);
    h.say("work on setting up that meeting now").await;

    let mut seen = Vec::new();
    while let Ok(Ok(Some(ev))) = tokio::time::timeout(Duration::from_millis(500), sub.next()).await {
        let done = matches!(ev, Event::TurnDone { .. });
        seen.push(ev);
        if done {
            break;
        }
    }
    assert!(
        seen.iter().any(|e| matches!(e, Event::Preview { kind, content, .. } if kind == "slack message" && content.contains("Thursday"))),
        "{seen:?}"
    );
    assert!(seen.iter().any(|e| matches!(e, Event::Reply { text, .. } if text == "Draft above.")));
    h.stop().await;
}

#[tokio::test]
async fn edit_move_and_remove_tasks() {
    let h = start(TASKS).await;
    h.llm.script([
        calls(&[
            ("edit_task", json!({"task_id": "t2", "title": "Deploy new billing docs", "section": "Design", "due": "2026-11-02"})),
            ("remove_task", json!({"task_id": "t3", "reason": "not the person's; the design thread was about someone else"})),
            ("add_task", json!({"section": "Today", "title": "Bad date", "due": "next week"})),
        ]),
        text("Updated."),
    ]);
    let events = h.say("is that really mine? I don't think I was on that thread").await;
    assert_eq!(changes(&events).len(), 2, "{events:?}");

    let list = h.tasks().await;
    let design = &list.sections[1];
    assert_eq!(design.tasks.len(), 1);
    assert_eq!(design.tasks[0].title, "Deploy new billing docs");
    assert_eq!(design.tasks[0].due.unwrap().to_string(), "2026-11-02");
    assert!(list.get("t3").is_err());
    let results = tool_results(&h.llm.requests()[1]);
    assert!(results[2].contains("YYYY-MM-DD"), "{results:?}");
    assert!(h.file("log.md").contains("removed t3: Review mockups"));
    h.stop().await;
}

#[tokio::test]
async fn read_only_lookups_stay_inside_repos_root() {
    let h = start_with(TASKS, |home| {
        let repos = home.join("repos");
        std::fs::create_dir_all(repos.join("api/src")).unwrap();
        std::fs::write(repos.join("api/src/routes.rs"), "fn routes() {\n    get(\"/v2/health\");\n}\n").unwrap();
        format!("\n[sources]\nrepos_root = \"{}\"\n", repos.display())
    })
    .await;
    h.llm.script([
        calls(&[
            ("read_repo", json!({"path": "api", "query": "v2/health"})),
            ("read_repo", json!({"path": "api/src/routes.rs"})),
            ("read_repo", json!({"path": "../.."})),
            ("search_source", json!({"source": "slack", "query": "questions"})),
            ("fetch_url", json!({"url": "https://myteam.slack.com/archives/C1/p123"})),
        ]),
        text("It's just new routes."),
    ]);
    h.say("how do we confirm the health route exists?").await;

    let results = tool_results(&h.llm.requests()[1]);
    assert!(results[0].contains("routes.rs") && results[0].contains("/v2/health"), "{results:?}");
    assert!(results[1].contains("    2      get(\"/v2/health\");"), "{results:?}");
    assert!(results[2].contains("outside repos_root"), "{results:?}");
    assert!(results[3].contains("isn't connected yet"), "{results:?}");
    assert!(results[4].contains("Slack adapter"), "{results:?}");
    h.stop().await;
}

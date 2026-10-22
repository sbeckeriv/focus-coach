//! The Claude backend's wire format, against a mock Messages endpoint: headers,
//! effort, fallbacks, prompt caching, and assistant content sent back unchanged.

mod support;

use std::time::Duration;

use focus_core::{Event, Paths, Request, client, daemon};
use serde_json::{Value, json};
use support::{MockLlm, Reply};

fn message(content: Value, stop_reason: &str) -> Reply {
    Reply::Ok(json!({"id": "msg_1", "type": "message", "role": "assistant", "content": content, "stop_reason": stop_reason}))
}

async fn start(llm: &MockLlm) -> (tempfile::TempDir, Paths) {
    // Only this test binary's process sees the key.
    unsafe { std::env::set_var("ANTHROPIC_API_KEY", "test-key") };
    let dir = tempfile::tempdir().unwrap();
    let base = llm.url.trim_end_matches("/v1");
    std::fs::write(
        dir.path().join("focus.toml"),
        format!("schedule = \"0 3 1 1 *\"\nnotifier = \"terminal\"\nmorning_bootstrap = \"\"\nreport_at = \"off\"\n[llm]\nprovider = \"claude\"\nbase_url = \"{base}\"\n"),
    )
    .unwrap();
    std::fs::write(dir.path().join("tasks.md"), "## Today\n- [ ] Ship it {id=t1}\n").unwrap();
    let paths = Paths { home: dir.path().to_path_buf() };
    tokio::spawn(daemon::run(paths.clone()));
    for _ in 0..100 {
        if client::is_running(&paths).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (dir, paths)
}

async fn say(paths: &Paths, text: &str) -> Vec<Event> {
    let mut events = Vec::new();
    client::request(paths, Request::Say { text: text.into() }, |e| events.push(e)).await.unwrap();
    events
}

#[tokio::test]
async fn messages_api_requests_and_tool_loop() {
    let llm = MockLlm::start().await;
    let (_dir, paths) = start(&llm).await;

    let thinking = json!({"type": "thinking", "thinking": "", "signature": "sig-abc"});
    llm.script([
        message(
            json!([thinking, {"type": "text", "text": "Checking it off."},
                   {"type": "tool_use", "id": "toolu_1", "name": "check_off", "input": {"task_id": "t1"}}]),
            "tool_use",
        ),
        message(json!([{"type": "text", "text": "Done."}]), "end_turn"),
        message(json!([{"type": "text", "text": "Yes, just routes."}]), "end_turn"),
        message(json!([]), "refusal"),
    ]);

    let events = say(&paths, "shipped it").await;
    assert!(events.iter().any(|e| matches!(e, Event::Reply { text, .. } if text == "Done.")), "{events:?}");

    let requests = llm.requests();
    let first = &requests[0];
    assert_eq!(llm.header(0, "anthropic-version").as_deref(), Some("2026-10-21"));
    assert_eq!(llm.header(0, "x-api-key").as_deref(), Some("test-key"));
    assert_eq!(first["model"], "claude-haiku-5-5");
    assert_eq!(first["output_config"]["effort"], "low");
    assert!(first.get("fallbacks").is_none(), "Haiku gets no server-side fallback");
    assert!(llm.header(0, "anthropic-beta").is_none());
    assert_eq!(first["system"][0]["cache_control"]["type"], "ephemeral");
    for banned in ["temperature", "top_p", "top_k", "tool_choice"] {
        assert!(first.get(banned).is_none(), "{banned} must not be sent");
    }
    assert!(first["tools"].as_array().unwrap().iter().any(|t| t["name"] == "check_off" && t["input_schema"]["type"] == "object"));

    // Second call: the assistant turn goes back exactly as received, thinking block included.
    let second = &requests[1];
    let messages = second["messages"].as_array().unwrap();
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"][0], thinking);
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(messages[2]["content"][0]["is_error"], false);

    // A design question goes to the strong model, with fallbacks on.
    say(&paths, "I'm still confused, is it just new routes or more work?").await;
    let third = &llm.requests()[2];
    assert_eq!(third["model"], "claude-opus-5-5");
    assert_eq!(third["output_config"]["effort"], "high");
    assert_eq!(third["fallbacks"], "default");
    assert_eq!(llm.header(2, "anthropic-beta").as_deref(), Some("server-side-fallback-2026-07-01"));

    // Refusals surface as errors rather than empty replies.
    let events = say(&paths, "ok").await;
    assert!(events.iter().any(|e| matches!(e, Event::Error { text, .. } if text.contains("declined"))), "{events:?}");

    client::request(&paths, Request::Shutdown, |_| {}).await.unwrap();
}

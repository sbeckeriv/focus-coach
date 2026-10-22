//! Shared by the core and CLI integration tests: a scripted OpenAI-compatible
//! server standing in for the model, and a throwaway FOCUS_HOME pointed at it.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Headers and JSON body of one request the fake model received.
type Recorded = (Vec<(String, String)>, Value);

/// One scripted reply from the fake model.
pub enum Reply {
    Ok(Value),
    Status(u16, String),
}

/// The model answers with text and no tool calls.
pub fn text(s: &str) -> Reply {
    Reply::Ok(json!({"choices": [{"message": {"role": "assistant", "content": s}, "finish_reason": "stop"}]}))
}

/// The model calls these tools, in order, with optional leading text.
pub fn calls(calls: &[(&str, Value)]) -> Reply {
    let tool_calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| {
            json!({"id": format!("call_{i}_{name}"), "type": "function",
                   "function": {"name": name, "arguments": args.to_string()}})
        })
        .collect();
    Reply::Ok(json!({"choices": [{"message": {"role": "assistant", "content": "", "tool_calls": tool_calls},
                                  "finish_reason": "tool_calls"}]}))
}

#[derive(Clone)]
pub struct MockLlm {
    pub url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    script: Arc<Mutex<VecDeque<Reply>>>,
}

impl MockLlm {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let mock = MockLlm { url, requests: Default::default(), script: Default::default() };
        let server = mock.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let server = server.clone();
                tokio::spawn(async move { server.serve(stream).await });
            }
        });
        mock
    }

    /// Queues replies; each chat-completions request takes the next one.
    pub fn script(&self, replies: impl IntoIterator<Item = Reply>) {
        self.script.lock().unwrap().extend(replies);
    }

    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().iter().map(|(_, b)| b.clone()).collect()
    }

    /// The value of header `name` (lowercase) on request `index`.
    pub fn header(&self, index: usize, name: &str) -> Option<String> {
        let requests = self.requests.lock().unwrap();
        requests[index].0.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
    }

    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }

    async fn serve(&self, stream: tokio::net::TcpStream) {
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        // Keep-alive: several requests may share one connection.
        loop {
            let mut length = 0usize;
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    let (name, value) = (name.trim().to_ascii_lowercase(), value.trim().to_string());
                    if name == "content-length" {
                        length = value.parse().unwrap_or(0);
                    }
                    headers.push((name, value));
                }
            }
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).await.is_err() {
                return;
            }
            self.requests.lock().unwrap().push((headers, serde_json::from_slice(&body).unwrap_or(Value::Null)));
            let reply = self.script.lock().unwrap().pop_front().unwrap_or_else(|| text("(script exhausted)"));
            let (status, body) = match reply {
                Reply::Ok(v) => (200, v.to_string()),
                Reply::Status(code, msg) => (code, json!({"error": {"message": msg}}).to_string()),
            };
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            if write.write_all(head.as_bytes()).await.is_err() || write.write_all(body.as_bytes()).await.is_err() {
                return;
            }
        }
    }
}

/// Writes a focus.toml that points at `llm_url` and never fires a check-in on its own.
pub fn write_config(home: &Path, llm_url: &str) {
    std::fs::create_dir_all(home).unwrap();
    let config = format!(
        r#"schedule = "0 3 1 1 *"
notifier = "terminal"
miss_threshold = 3

[llm]
provider = "openai"
base_url = "{llm_url}"
light_model = "mock-light"
strong_model = "mock-strong"
"#
    );
    std::fs::write(home.join("focus.toml"), config).unwrap();
}

/// The text of the user message in a recorded request (the framed context + input).
pub fn user_prompt(request: &Value) -> String {
    request["messages"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["role"] == "user"))
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// Tool results sent back to the model in a recorded request.
pub fn tool_results(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str().map(String::from))
        .collect()
}

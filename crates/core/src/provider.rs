//! LLM backends. Prompts and tool schemas are written once in the Claude Messages
//! format; other backends translate.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::config::LlmSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Check-ins and short replies.
    Light,
    /// Building the list, long docs, design questions, reports.
    Strong,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone)]
pub struct Turn {
    /// The assistant content blocks exactly as returned, to send back unchanged.
    pub content: Vec<Value>,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: String,
}

pub enum Provider {
    Claude(Claude),
    Local(Local),
    ClaudeCode(ClaudeCode),
}

impl Provider {
    pub fn from_settings(llm: &LlmSettings) -> Result<Self> {
        let default_url = match llm.provider.as_str() {
            "claude" => return Ok(Provider::Claude(Claude::new(llm)?)),
            "claude-code" | "claude_code" => return Ok(Provider::ClaudeCode(ClaudeCode::new(llm))),
            "ollama" => "http://localhost:11434/v1",
            "lmstudio" => "http://localhost:1234/v1",
            "openai" | "local" | "llamacpp" => "http://localhost:8080/v1",
            other => bail!("unknown llm.provider {other:?}; use claude, claude-code, ollama, lmstudio or openai"),
        };
        if llm.light_model.starts_with("claude-") || llm.strong_model.starts_with("claude-") {
            bail!(
                "llm.provider is {:?} but the models are Claude models; set llm.light_model and llm.strong_model to local model names",
                llm.provider
            );
        }
        Ok(Provider::Local(Local::new(llm, default_url)?))
    }

    pub async fn turn(&self, tier: Tier, system: &str, tools: &[Value], messages: &[Value]) -> Result<Turn> {
        match self {
            Provider::Claude(c) => c.turn(tier, system, tools, messages).await,
            Provider::Local(l) => l.turn(tier, system, tools, messages).await,
            Provider::ClaudeCode(c) => {
                if !tools.is_empty() {
                    bail!("claude-code runs its own tool loop; use ClaudeCode::agent");
                }
                let text = c.run(tier, system, &flatten(messages), None, None, None).await?;
                Ok(Turn { content: vec![json!({"type": "text", "text": text})], text, tool_calls: vec![], stop_reason: "end_turn".into() })
            }
        }
    }
}

pub struct Claude {
    http: reqwest::Client,
    base_url: String,
    light: (String, String),
    strong: (String, String),
}

/// Models that accept server-side refusal fallbacks (`fallbacks: "default"`).
const FALLBACK_MODELS: &[&str] = &["claude-opus-5-5", "claude-opus-5", "claude-fable-5-1", "claude-sonnet-5-5"];

impl Claude {
    fn new(llm: &LlmSettings) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(Duration::from_secs(600)).build()?,
            base_url: llm.base_url.as_deref().unwrap_or("https://api.anthropic.com").trim_end_matches('/').to_string(),
            light: (llm.light_model.clone(), llm.light_effort.clone()),
            strong: (llm.strong_model.clone(), llm.strong_effort.clone()),
        })
    }

    async fn turn(&self, tier: Tier, system: &str, tools: &[Value], messages: &[Value]) -> Result<Turn> {
        let (model, effort) = match tier {
            Tier::Light => &self.light,
            Tier::Strong => &self.strong,
        };
        let mut body = json!({
            "model": model,
            "max_tokens": 16000,
            "system": [{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}],
            "messages": messages,
            "output_config": {"effort": effort},
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
        }
        let mut betas = Vec::new();
        if FALLBACK_MODELS.contains(&model.as_str()) {
            body["fallbacks"] = json!("default");
            betas.push("server-side-fallback-2026-07-01");
        }

        let response = self.send(&body, betas).await?;
        let stop_reason = response["stop_reason"].as_str().unwrap_or_default().to_string();
        if stop_reason == "refusal" {
            bail!("the model declined this request");
        }
        let content = response["content"].as_array().cloned().unwrap_or_default();
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in &content {
            match block["type"].as_str() {
                Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
                Some("tool_use") => tool_calls.push(ToolCall {
                    id: block["id"].as_str().unwrap_or_default().to_string(),
                    name: block["name"].as_str().unwrap_or_default().to_string(),
                    input: block["input"].clone(),
                }),
                _ => {}
            }
        }
        Ok(Turn { content, text, tool_calls, stop_reason })
    }

    async fn send(&self, body: &Value, mut betas: Vec<&'static str>) -> Result<Value> {
        let auth = auth().await?;
        if matches!(auth, Auth::Bearer(_)) {
            betas.push("oauth-2025-04-20");
        }
        let mut attempt = 0;
        loop {
            let mut req = self
                .http
                .post(format!("{}/v1/messages", self.base_url))
                .header("anthropic-version", "2026-10-21")
                .json(body);
            req = match &auth {
                Auth::Key(k) => req.header("x-api-key", k),
                Auth::Bearer(t) => req.bearer_auth(t),
            };
            if !betas.is_empty() {
                req = req.header("anthropic-beta", betas.join(","));
            }
            let resp = req.send().await.context("calling the Claude API")?;
            let status = resp.status();
            let retryable = status.as_u16() == 429 || status.as_u16() == 529 || status.is_server_error();
            if retryable && attempt < 2 {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
                    .unwrap_or(2u64.pow(attempt + 1));
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(wait.min(30))).await;
                continue;
            }
            let raw = resp.text().await.context("reading the Claude API response")?;
            let value: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({"error": {"message": raw}}));
            if !status.is_success() {
                let msg = value["error"]["message"].as_str().unwrap_or("unknown error");
                bail!("Claude API {status}: {msg}");
            }
            return Ok(value);
        }
    }
}

/// Any OpenAI-compatible chat-completions server with tool calling: Ollama,
/// LM Studio, llama.cpp, vLLM. Messages are kept in Claude's block format and
/// translated on the way in and out.
pub struct Local {
    http: reqwest::Client,
    base_url: String,
    light: String,
    strong: String,
    api_key: Option<String>,
}

impl Local {
    fn new(llm: &LlmSettings, default_url: &str) -> Result<Self> {
        let api_key = match &llm.api_key_env {
            Some(var) => Some(std::env::var(var).with_context(|| format!("llm.api_key_env names {var}, which isn't set"))?),
            None => None,
        };
        Ok(Self {
            // Local models can be slow on a laptop.
            http: reqwest::Client::builder().timeout(Duration::from_secs(900)).build()?,
            base_url: llm.base_url.as_deref().unwrap_or(default_url).trim_end_matches('/').to_string(),
            light: llm.light_model.clone(),
            strong: llm.strong_model.clone(),
            api_key,
        })
    }

    async fn turn(&self, tier: Tier, system: &str, tools: &[Value], messages: &[Value]) -> Result<Turn> {
        let model = match tier {
            Tier::Light => &self.light,
            Tier::Strong => &self.strong,
        };
        let mut body = json!({"model": model, "messages": to_openai_messages(system, messages)});
        if !tools.is_empty() {
            body["tools"] = to_openai_tools(tools);
        }
        let url = format!("{}/chat/completions", self.base_url);
        let mut req = self.http.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.with_context(|| format!("calling {url} (is the local model server running?)"))?;
        let status = resp.status();
        let raw = resp.text().await?;
        let value: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({"error": {"message": raw}}));
        if !status.is_success() {
            let msg = value["error"]["message"].as_str().or(value["error"].as_str()).unwrap_or("unknown error");
            bail!("{url} {status}: {msg}");
        }
        let choice = &value["choices"][0];
        Ok(from_openai_message(&choice["message"], choice["finish_reason"].as_str().unwrap_or("")))
    }
}

fn to_openai_tools(tools: &[Value]) -> Value {
    tools
        .iter()
        .map(|t| {
            json!({"type": "function", "function": {
                "name": t["name"],
                "description": t["description"],
                "parameters": t["input_schema"],
            }})
        })
        .collect()
}

fn to_openai_messages(system: &str, messages: &[Value]) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": system})];
    for message in messages {
        let role = message["role"].as_str().unwrap_or("user");
        let blocks = match &message["content"] {
            Value::String(text) => {
                out.push(json!({"role": role, "content": text}));
                continue;
            }
            Value::Array(blocks) => blocks,
            _ => continue,
        };
        if role == "assistant" {
            let text: String = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
            let calls: Vec<Value> = blocks
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| {
                    json!({"id": b["id"], "type": "function", "function": {
                        "name": b["name"],
                        "arguments": b["input"].to_string(),
                    }})
                })
                .collect();
            let mut msg = json!({"role": "assistant", "content": text});
            if !calls.is_empty() {
                msg["tool_calls"] = Value::Array(calls);
            }
            out.push(msg);
            continue;
        }
        for block in blocks {
            match block["type"].as_str() {
                Some("tool_result") => out.push(json!({
                    "role": "tool",
                    "tool_call_id": block["tool_use_id"],
                    "content": block["content"].as_str().map_or_else(|| block["content"].to_string(), String::from),
                })),
                Some("text") => out.push(json!({"role": role, "content": block["text"]})),
                _ => {}
            }
        }
    }
    out
}

fn from_openai_message(msg: &Value, finish_reason: &str) -> Turn {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let text = strip_think(msg["content"].as_str().unwrap_or_default());
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    let mut tool_calls = Vec::new();
    for call in msg["tool_calls"].as_array().into_iter().flatten() {
        let id = match call["id"].as_str() {
            Some(id) if !id.is_empty() => id.to_string(),
            // Ollama sometimes omits ids.
            _ => format!("call_{}", NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
        };
        let name = call["function"]["name"].as_str().unwrap_or_default().to_string();
        let input = match &call["function"]["arguments"] {
            Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({"_unparsed_arguments": s})),
            Value::Null => json!({}),
            other => other.clone(),
        };
        content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
        tool_calls.push(ToolCall { id, name, input });
    }
    let stop_reason = match finish_reason {
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        _ if !tool_calls.is_empty() => "tool_use",
        _ => "end_turn",
    };
    Turn { content, text, tool_calls, stop_reason: stop_reason.into() }
}

/// Reasoning models served locally often put their reasoning inline in `<think>` tags.
fn strip_think(text: &str) -> String {
    let mut rest = text;
    let mut out = String::new();
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end) => rest = &rest[start + end + "</think>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

enum Auth {
    Key(String),
    Bearer(String),
}

/// `ANTHROPIC_API_KEY`, then `ANTHROPIC_AUTH_TOKEN`, then the `ant` CLI's active profile.
async fn auth() -> Result<Auth> {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && !key.is_empty()
    {
        return Ok(Auth::Key(key));
    }
    if let Ok(token) = std::env::var("ANTHROPIC_AUTH_TOKEN")
        && !token.is_empty()
    {
        return Ok(Auth::Bearer(token));
    }
    let out = tokio::process::Command::new("ant")
        .args(["auth", "print-credentials", "--access-token"])
        .output()
        .await;
    match out {
        Ok(out) if out.status.success() => {
            let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if token.is_empty() {
                bail!("`ant auth print-credentials` returned no token");
            }
            Ok(Auth::Bearer(token))
        }
        _ => bail!("no Claude credentials: set ANTHROPIC_API_KEY or run `ant auth login`"),
    }
}

/// Runs turns through the Claude Code CLI on the person's Claude Code login. Claude Code
/// drives the tool loop itself: its built-in tools plus the coach's, served by `focus mcp`.
pub struct ClaudeCode {
    command: Option<String>,
    light: (String, String),
    strong: (String, String),
}

/// Claude Code's built-ins the coach may use without asking, next to `mcp__focus`.
const CLAUDE_CODE_TOOLS: &[&str] =
    &["Bash", "Read", "Edit", "Write", "Glob", "Grep", "WebFetch", "WebSearch", "mcp__focus"];

impl ClaudeCode {
    fn new(llm: &LlmSettings) -> Self {
        Self {
            command: llm.command.clone(),
            light: (llm.light_model.clone(), llm.light_effort.clone()),
            strong: (llm.strong_model.clone(), llm.strong_effort.clone()),
        }
    }

    /// `claude` on PATH, else the installer's ~/.local/bin (apps opened from Finder have a short PATH).
    fn program(&self) -> PathBuf {
        if let Some(c) = &self.command {
            return c.into();
        }
        let on_path = std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("claude").is_file()));
        match dirs::home_dir().map(|h| h.join(".local/bin/claude")) {
            Some(local) if !on_path && local.is_file() => local,
            _ => "claude".into(),
        }
    }

    /// One agent turn with every tool. `mcp_server` is the command line for `focus mcp`.
    pub(crate) async fn agent(
        &self,
        tier: Tier,
        system: &str,
        prompt: &str,
        mcp_server: (&Path, Vec<String>, &Path),
        cwd: &Path,
        session: (&str, bool),
    ) -> Result<String> {
        let (exe, args, home) = mcp_server;
        let config = json!({"mcpServers": {"focus": {
            "type": "stdio",
            "command": exe,
            "args": args,
            "env": {"FOCUS_HOME": home},
        }}});
        let (id, resume) = session;
        let config = config.to_string();
        match self.run(tier, system, prompt, Some(&config), Some(cwd), Some((id, resume))).await {
            // Our record of the session can be wrong (a configured id, deleted sessions): try the other way once.
            Err(e) if format!("{e:#}").contains(if resume { "No conversation found" } else { "already in use" }) => {
                self.run(tier, system, prompt, Some(&config), Some(cwd), Some((id, !resume))).await
            }
            other => other,
        }
    }

    /// Runs `claude -p` once. Without an MCP config it gets no tools at all; without a
    /// session (id, resume) nothing is saved.
    async fn run(
        &self,
        tier: Tier,
        system: &str,
        prompt: &str,
        mcp: Option<&str>,
        cwd: Option<&Path>,
        session: Option<(&str, bool)>,
    ) -> Result<String> {
        let (model, effort) = match tier {
            Tier::Light => &self.light,
            Tier::Strong => &self.strong,
        };
        let program = self.program();
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(["-p", "--output-format", "json", "--model", model]);
        match session {
            Some((id, true)) => cmd.args(["--resume", id]),
            Some((id, false)) => cmd.args(["--session-id", id]),
            None => cmd.arg("--no-session-persistence"),
        };
        cmd.args(["--system-prompt", system, "--strict-mcp-config"]);
        if !effort.is_empty() {
            cmd.args(["--effort", effort]);
        }
        match mcp {
            Some(config) => {
                cmd.args(["--mcp-config", config, "--allowedTools"]).args(CLAUDE_CODE_TOOLS);
            }
            None => {
                cmd.args(["--tools", ""]);
            }
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("running {} (is Claude Code installed?)", program.display()))?;
        let mut stdin = child.stdin.take().context("claude stdin")?;
        stdin.write_all(prompt.as_bytes()).await?;
        drop(stdin);
        let output = tokio::time::timeout(Duration::from_secs(900), child.wait_with_output())
            .await
            .context("claude -p timed out after 15 minutes")??;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let result: Value = stdout
            .lines()
            .rev()
            .find_map(|l| serde_json::from_str(l).ok())
            .with_context(|| {
                let stderr = String::from_utf8_lossy(&output.stderr);
                format!("claude -p gave no result ({}): {} {}", output.status, stderr.trim(), stdout.trim())
            })?;
        let text = result["result"].as_str().unwrap_or_default().to_string();
        if result["is_error"].as_bool().unwrap_or(!output.status.success()) {
            bail!("claude -p failed: {}", if text.is_empty() { result.to_string() } else { text });
        }
        Ok(text)
    }
}

/// Message text in order, for a CLI that takes one prompt.
fn flatten(messages: &[Value]) -> String {
    let text = |c: &Value| match c {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    messages.iter().map(|m| text(&m["content"])).collect::<Vec<_>>().join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_round_trip() {
        let reply = json!({
            "content": "<think>hmm</think>Checking.",
            "tool_calls": [
                {"id": "", "function": {"name": "check_off", "arguments": "{\"task_id\":\"t3\"}"}},
                {"id": "abc", "function": {"name": "append_log", "arguments": {"summary": "on t3"}}},
            ],
        });
        let turn = from_openai_message(&reply, "tool_calls");
        assert_eq!(turn.text, "Checking.");
        assert_eq!(turn.stop_reason, "tool_use");
        assert_eq!(turn.tool_calls[0].input["task_id"], "t3");
        assert_eq!(turn.tool_calls[1].input["summary"], "on t3");

        let messages = vec![
            json!({"role": "user", "content": "done with t3"}),
            json!({"role": "assistant", "content": turn.content}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": turn.tool_calls[0].id, "content": "checked off t3"},
                {"type": "tool_result", "tool_use_id": "abc", "content": "logged"},
            ]}),
        ];
        let out = to_openai_messages("sys", &messages);
        assert_eq!(out.len(), 5);
        assert_eq!(out[2]["tool_calls"][0]["function"]["arguments"], "{\"task_id\":\"t3\"}");
        assert_eq!(out[3]["role"], "tool");
        assert_eq!(out[3]["tool_call_id"], turn.tool_calls[0].id.as_str());
        assert_eq!(out[4]["tool_call_id"], "abc");
    }

    #[test]
    fn local_provider_rejects_claude_models() {
        let llm = LlmSettings { provider: "ollama".into(), ..Default::default() };
        assert!(Provider::from_settings(&llm).is_err());
        let llm = LlmSettings { provider: "ollama".into(), light_model: "qwen3:8b".into(), strong_model: "qwen3:32b".into(), ..Default::default() };
        assert!(matches!(Provider::from_settings(&llm), Ok(Provider::Local(_))));
    }
}

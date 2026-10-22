//! `focus mcp <turn>`: a stdio MCP server that Claude Code starts for one
//! claude-code turn. It lists the coach's tools and forwards each call to the daemon.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::config::Paths;
use crate::protocol::{Event, Request};
use crate::{client, tools};

/// Serves newline-delimited JSON-RPC on stdin and stdout until stdin closes.
pub async fn serve_stdio(paths: Paths, turn: u64) -> Result<()> {
    serve(&paths, turn, BufReader::new(tokio::io::stdin()), tokio::io::stdout()).await
}

pub async fn serve(paths: &Paths, turn: u64, input: impl AsyncBufRead + Unpin, mut out: impl AsyncWrite + Unpin) -> Result<()> {
    let mut lines = input.lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        // Notifications carry no id and get no answer.
        let Some(id) = msg.get("id").cloned() else { continue };
        let reply = match handle(paths, turn, &msg).await {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("{e:#}")}}),
        };
        let mut text = reply.to_string();
        text.push('\n');
        out.write_all(text.as_bytes()).await?;
        out.flush().await?;
    }
    Ok(())
}

async fn handle(paths: &Paths, turn: u64, msg: &Value) -> Result<Value> {
    let params = &msg["params"];
    Ok(match msg["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "focus", "version": env!("CARGO_PKG_VERSION")},
        }),
        "ping" => json!({}),
        "tools/list" => {
            let tools: Vec<Value> = tools::definitions()
                .into_iter()
                .map(|t| json!({"name": t["name"], "description": t["description"], "inputSchema": t["input_schema"]}))
                .collect();
            json!({"tools": tools})
        }
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default().to_string();
            let input = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let (content, is_error) = call(paths, turn, name, input).await;
            json!({"content": [{"type": "text", "text": content}], "isError": is_error})
        }
        other => bail!("method {other:?} not supported"),
    })
}

/// Runs one tool in the daemon, inside the turn that started this server.
async fn call(paths: &Paths, turn: u64, name: String, input: Value) -> (String, bool) {
    let mut answer = None;
    let sent = client::request(paths, Request::Tool { turn, name, input }, |ev| match ev {
        Event::ToolResult { content, is_error } => answer = Some((content, is_error)),
        Event::Error { text, .. } => answer = Some((format!("error: {text}"), true)),
        _ => {}
    })
    .await;
    match (sent, answer) {
        (_, Some(answer)) => answer,
        (Err(e), None) => (format!("error: {e:#}"), true),
        (Ok(()), None) => ("error: the daemon sent no result".into(), true),
    }
}

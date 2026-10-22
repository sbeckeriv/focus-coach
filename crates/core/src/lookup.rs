//! Read-only lookups: GitHub through the `gh` CLI, web pages over HTTP, local code.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::Sources;

const MAX_CHARS: usize = 40_000;

async fn run_cmd(program: &str, args: &[&str], ok_codes: &[i32]) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(45),
        tokio::process::Command::new(program).args(args).kill_on_drop(true).output(),
    )
    .await
    .with_context(|| format!("{program} timed out"))?
    .with_context(|| format!("running {program}"))?;
    let code = output.status.code().unwrap_or(-1);
    if !output.status.success() && !ok_codes.contains(&code) {
        bail!("{program} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Posts a comment on a PR or issue. Only reached from an approved action.
pub async fn comment_on_github(url: &str, body: &str) -> Result<String> {
    let (host, path) = host_and_path(url);
    if host != "github.com" {
        bail!("{url} isn't a GitHub link");
    }
    let kind = if path.contains("/pull/") { "pr" } else if path.contains("/issues/") { "issue" } else { bail!("{url} isn't a PR or issue") };
    let out = run_cmd("gh", &[kind, "comment", url, "--body", body], &[]).await?;
    Ok(format!("Posted. {}", out.trim()))
}

fn cap(mut s: String) -> String {
    if s.len() > MAX_CHARS {
        let mut end = MAX_CHARS;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("\n[truncated]");
    }
    s
}

fn host_and_path(url: &str) -> (String, String) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    (host.to_ascii_lowercase(), format!("/{path}"))
}

enum GithubKind {
    Pull,
    Issue,
}

fn github_kind(url: &str) -> Option<GithubKind> {
    let (host, path) = host_and_path(url);
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    match parts.get(2) {
        Some(&"pull") => Some(GithubKind::Pull),
        Some(&"issues") => Some(GithubKind::Issue),
        _ => None,
    }
}

pub async fn fetch_url(url: &str) -> Result<String> {
    match github_kind(url) {
        Some(GithubKind::Pull) => {
            let fields = "number,title,state,isDraft,author,url,body,baseRefName,headRefName,mergedAt,reviewDecision,reviewRequests,comments,reviews";
            return Ok(cap(run_cmd("gh", &["pr", "view", url, "--json", fields], &[]).await?));
        }
        Some(GithubKind::Issue) => {
            let fields = "number,title,state,author,url,body,labels,assignees,comments";
            return Ok(cap(run_cmd("gh", &["issue", "view", url, "--json", fields], &[]).await?));
        }
        None => {}
    }
    let (host, _) = host_and_path(url);
    for (needle, name) in [("slack.com", "Slack"), ("notion.so", "Notion"), ("notion.site", "Notion"), ("atlassian.net", "Jira")] {
        if host.ends_with(needle) {
            bail!("{name} links need the {name} adapter, which isn't connected yet. Ask the person to paste the relevant text.");
        }
    }
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .get(url)
        .header("user-agent", "focus-coach/0.1")
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    let status = resp.status();
    let is_html = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("html"));
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("{url} returned {status}");
    }
    Ok(cap(if is_html { html_to_text(&body) } else { body }))
}

/// Good enough to read an article or docs page: drops scripts, styles and tags.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 3);
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < html.len() {
        let rest = &lower[i..];
        if rest.starts_with("<script") || rest.starts_with("<style") {
            let close = if rest.starts_with("<script") { "</script>" } else { "</style>" };
            i += rest.find(close).map_or(rest.len(), |p| p + close.len());
            continue;
        }
        if rest.starts_with('<') {
            let end = rest.find('>').map_or(rest.len(), |p| p + 1);
            let tag = &rest[..end];
            if ["<br", "<p", "</p", "<div", "</div", "<li", "<h", "</h", "<tr"].iter().any(|t| tag.starts_with(t)) {
                out.push('\n');
            }
            i += end;
            continue;
        }
        let ch = html[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut text = String::new();
    for line in decoded.lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")) {
        if !line.is_empty() {
            text.push_str(&line);
            text.push('\n');
        }
    }
    text
}

/// Accepts a PR URL or `owner/repo#123`.
pub async fn pr_status(pr: &str) -> Result<String> {
    let fields = "number,title,url,state,isDraft,reviewDecision,mergeable,mergedAt,reviewRequests,statusCheckRollup";
    let raw = match pr.split_once('#') {
        Some((repo, number)) if !repo.is_empty() && !pr.starts_with("http") => {
            run_cmd("gh", &["pr", "view", number, "-R", repo, "--json", fields], &[]).await?
        }
        _ if pr.starts_with("http") => run_cmd("gh", &["pr", "view", pr, "--json", fields], &[]).await?,
        _ => bail!("give a PR URL or owner/repo#number (a bare number doesn't say which repo)"),
    };
    let mut value: Value = serde_json::from_str(&raw)?;
    let checks = value["statusCheckRollup"].as_array().cloned().unwrap_or_default();
    let (mut passing, mut pending, mut failing) = (0, 0, Vec::new());
    for check in &checks {
        let result = check["conclusion"].as_str().or(check["state"].as_str()).unwrap_or("").to_ascii_uppercase();
        let name = check["name"].as_str().or(check["context"].as_str()).unwrap_or("check");
        match result.as_str() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => passing += 1,
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
                failing.push(name.to_string())
            }
            _ => pending += 1,
        }
    }
    value["statusCheckRollup"] = json!({"passing": passing, "pending": pending, "failing": failing});
    Ok(serde_json::to_string_pretty(&value)?)
}

pub async fn search(sources: &Sources, source: &str, query: &str, filters: &Value) -> Result<String> {
    match source {
        "github" => search_github(sources, query, filters).await,
        "repos" => {
            if query.is_empty() {
                bail!("repos search needs a query");
            }
            let path = filters["path"].as_str().unwrap_or(".");
            read_repo(sources, path, Some(query)).await
        }
        "slack" | "notion" | "jira" => {
            bail!("the {source} adapter isn't connected yet. Ask the person to paste what they're looking at.")
        }
        other => bail!("unknown source {other:?}"),
    }
}

async fn search_github(sources: &Sources, query: &str, filters: &Value) -> Result<String> {
    let issues = filters["kind"].as_str() == Some("issues");
    let mut args: Vec<String> = vec!["search".into(), if issues { "issues" } else { "prs" }.into()];
    if !query.is_empty() {
        args.push(query.into());
    }
    for (key, flag) in [
        ("author", "--author"),
        ("review_requested", "--review-requested"),
        ("involves", "--involves"),
        ("assignee", "--assignee"),
        ("owner", "--owner"),
        ("repo", "--repo"),
    ] {
        if let Some(v) = filters[key].as_str() {
            args.extend([flag.to_string(), v.to_string()]);
        }
    }
    match filters["state"].as_str() {
        Some("merged") if !issues => args.push("--merged".into()),
        Some(state @ ("open" | "closed")) => args.extend(["--state".into(), state.into()]),
        _ => {}
    }
    if filters["owner"].is_null() && filters["repo"].is_null() && let Some(org) = &sources.github_org {
        args.extend(["--owner".into(), org.clone()]);
    }
    args.extend(["--limit".into(), "30".into(), "--json".into(), "number,title,url,state,repository,author,updatedAt".into()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(cap(run_cmd("gh", &refs, &[]).await?))
}

fn resolve(sources: &Sources, path: &str) -> Result<std::path::PathBuf> {
    let root = sources
        .repos_root
        .as_ref()
        .context("sources.repos_root isn't set in focus.toml")?
        .canonicalize()
        .context("sources.repos_root doesn't exist")?;
    let candidate = if Path::new(path).is_absolute() { Path::new(path).to_path_buf() } else { root.join(path) };
    let resolved = candidate.canonicalize().with_context(|| format!("{path} doesn't exist under repos_root"))?;
    if !resolved.starts_with(&root) {
        bail!("{path} is outside repos_root");
    }
    Ok(resolved)
}

pub async fn read_repo(sources: &Sources, path: &str, query: Option<&str>) -> Result<String> {
    let target = resolve(sources, path)?;
    let target_str = target.to_string_lossy().into_owned();
    if let Some(query) = query {
        let rg = run_cmd(
            "rg",
            &["-n", "--no-heading", "-S", "--max-count", "20", "--max-columns", "300", "-e", query, &target_str],
            &[1],
        )
        .await;
        let out = match rg {
            Ok(out) => out,
            Err(_) => {
                run_cmd(
                    "grep",
                    &["-rnI", "--exclude-dir=.git", "--exclude-dir=target", "--exclude-dir=node_modules", "-m", "20", "-e", query, &target_str],
                    &[1],
                )
                .await?
            }
        };
        return Ok(if out.is_empty() { "no matches".into() } else { cap(out) });
    }
    if target.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(&target)?
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() { format!("{name}/") } else { name }
            })
            .collect();
        names.sort();
        names.truncate(300);
        return Ok(names.join("\n"));
    }
    let text = std::fs::read_to_string(&target).with_context(|| format!("reading {path}"))?;
    let numbered: String = text.lines().take(2000).enumerate().map(|(i, l)| format!("{:>5}  {l}\n", i + 1)).collect();
    Ok(cap(numbered))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html() {
        let text = html_to_text("<html><style>x{}</style><p>Hello&nbsp;<b>there</b></p><script>bad()</script><li>one</li>");
        assert_eq!(text, "Hello there\none\n");
    }

    #[test]
    fn github_urls() {
        assert!(matches!(github_kind("https://github.com/o/r/pull/12"), Some(GithubKind::Pull)));
        assert!(matches!(github_kind("https://github.com/o/r/issues/3"), Some(GithubKind::Issue)));
        assert!(github_kind("https://example.com/o/r/pull/12").is_none());
    }
}

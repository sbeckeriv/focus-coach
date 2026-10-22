use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Where everything lives. `FOCUS_HOME` overrides the default `~/.focus`.
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let home = match std::env::var_os("FOCUS_HOME") {
            Some(h) => PathBuf::from(h),
            None => dirs::home_dir().context("no home directory")?.join(".focus"),
        };
        Ok(Self { home })
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(self.reports())
            .with_context(|| format!("creating {}", self.home.display()))
    }

    pub fn config(&self) -> PathBuf {
        self.home.join("focus.toml")
    }
    pub fn tasks(&self) -> PathBuf {
        self.home.join("tasks.md")
    }
    pub fn log(&self) -> PathBuf {
        self.home.join("log.md")
    }
    pub fn state(&self) -> PathBuf {
        self.home.join("state.json")
    }
    /// Unix socket paths are capped near 104 bytes, so a deep `FOCUS_HOME` gets one in /tmp.
    pub fn socket(&self) -> PathBuf {
        let path = self.home.join("focus.sock");
        if path.as_os_str().len() < 100 {
            return path;
        }
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.home.hash(&mut hasher);
        PathBuf::from(format!("/tmp/focus-{:016x}.sock", hasher.finish()))
    }
    pub fn reports(&self) -> PathBuf {
        self.home.join("reports")
    }
    pub fn daemon_log(&self) -> PathBuf {
        self.home.join("daemon.log")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Cron expression: minute hour day-of-month month day-of-week.
    pub schedule: String,
    /// Free-form, e.g. "9-17 Mon-Fri". Given to the model when it changes the schedule.
    pub work_hours: Option<String>,
    pub miss_threshold: u32,
    pub notifier: Notifier,
    pub llm: LlmSettings,
    pub sources: Sources,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schedule: "7,27,47 9-17 * * 1-5".into(),
            work_hours: Some("9-17 Mon-Fri".into()),
            miss_threshold: 3,
            notifier: Notifier::Desktop,
            llm: LlmSettings::default(),
            sources: Sources::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Notifier {
    Terminal,
    Desktop,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmSettings {
    /// "claude" (the API), "claude-code" (shells out to `claude -p`), or "ollama" /
    /// "lmstudio" / "openai" for any OpenAI-compatible server.
    pub provider: String,
    /// Defaults per provider: api.anthropic.com, localhost:11434/v1, localhost:1234/v1.
    pub base_url: Option<String>,
    /// Environment variable holding a bearer token for OpenAI-compatible servers, if any.
    pub api_key_env: Option<String>,
    /// The `claude` binary for provider = "claude-code"; found on PATH or in ~/.local/bin if unset.
    pub command: Option<String>,
    /// provider = "claude-code": one Claude Code session to resume on every turn. Unset,
    /// the daemon starts a new session each day.
    pub session_id: Option<String>,
    /// Check-ins and short replies.
    pub light_model: String,
    pub light_effort: String,
    /// Building the list, long docs, design questions, reports.
    pub strong_model: String,
    pub strong_effort: String,
}

impl Default for LlmSettings {
    fn default() -> Self {
        Self {
            provider: "claude".into(),
            base_url: None,
            api_key_env: None,
            command: None,
            session_id: None,
            light_model: "claude-haiku-5-5".into(),
            light_effort: "low".into(),
            strong_model: "claude-opus-5-5".into(),
            strong_effort: "high".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Sources {
    pub github_org: Option<String>,
    pub slack_user_id: Option<String>,
    pub notion_pages: Vec<String>,
    pub repos_root: Option<PathBuf>,
}

const DEFAULT_CONFIG: &str = r#"# Focus Coach settings. Delete a line to fall back to its default.

schedule = "7,27,47 9-17 * * 1-5"   # minute hour day-of-month month day-of-week
work_hours = "9-17 Mon-Fri"
miss_threshold = 3                  # unanswered check-ins before offering "say pause"
notifier = "desktop"                # "desktop" (macOS notification) or "terminal"

[llm]
provider = "claude"
light_model = "claude-haiku-5-5"
light_effort = "low"
strong_model = "claude-opus-5-5"
strong_effort = "high"

# A local model instead (any OpenAI-compatible server with tool calling):
# provider = "ollama"            # or "lmstudio", or "openai" with base_url set
# base_url = "http://localhost:11434/v1"
# light_model = "qwen3:8b"
# strong_model = "qwen3:32b"
# api_key_env = "MY_SERVER_TOKEN"  # only if the server wants a bearer token

# Or run each turn through Claude Code (`claude -p`) on your Claude Code login, with
# its built-in tools (Bash, Read, Edit, WebFetch...) next to the coach's own:
# provider = "claude-code"
# command = "/path/to/claude"     # if it isn't on PATH or in ~/.local/bin
# session_id = "<uuid>"           # resume one session forever; unset = a new session each day

[sources]
# github_org = "my-org"
# slack_user_id = "U012345"
# notion_pages = ["https://www.notion.so/..."]
# repos_root = "/Users/me/src"
"#;

impl Settings {
    /// Reads `focus.toml`, writing the commented default on first run.
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config();
        if !path.exists() {
            std::fs::write(&path, DEFAULT_CONFIG)
                .with_context(|| format!("writing {}", path.display()))?;
        }
        let text = std::fs::read_to_string(&path)?;
        let mut settings: Settings =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if let Some(root) = &settings.sources.repos_root
            && let Some(rest) = root.to_str().and_then(|s| s.strip_prefix("~/"))
            && let Some(home) = dirs::home_dir()
        {
            settings.sources.repos_root = Some(home.join(rest));
        }
        Ok(settings)
    }
}

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::NaiveTime;
use serde::Deserialize;

use crate::hours::WorkHours;
use crate::schedule::Cron;

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
    /// Finished tasks, moved out of tasks.md after `archive_after_days`.
    pub fn done(&self) -> PathBuf {
        self.home.join("done.md")
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
    pub schedule: Cron,
    /// e.g. "9-17 Mon-Fri" or "9:30-17:30 Mon,Tue,Thu". Check-ins only fire inside these
    /// hours, and the end-of-day report runs at the end. "" (None) means any time.
    #[serde(deserialize_with = "crate::hours::deserialize")]
    pub work_hours: Option<WorkHours>,
    pub miss_threshold: u32,
    /// Minutes before an unanswered check-in is sent again, with a sound. 0 turns it off.
    pub remind_after_minutes: u32,
    /// The menubar app opens its popover, not just a notification, once this many check-ins
    /// in a row are waiting on a reply: 1 is every check-in, 0 never. `true`/`false` mean 1/0.
    #[serde(deserialize_with = "popover_after")]
    pub checkin_popover: u32,
    pub notifier: Notifier,
    /// Days a done item stays on the list before moving to done.md; 0 moves it right away.
    pub archive_after_days: u32,
    /// Days of log.md to keep; older entries are deleted. 0 keeps everything.
    pub log_keep_days: u32,
    /// When the end-of-day report runs on work days: "end" (end of work hours), "HH:MM", or "off".
    pub report_at: ReportAt,
    /// Days of log and done items the report covers.
    pub report_lookback_days: u32,
    /// Run at the first message or check-in of each day, before anything else. Empty turns it off.
    pub morning_bootstrap: String,
    /// Run right before each check-in to pick up progress and new work from your sources,
    /// e.g. "check my PRs and Slack for progress on my tasks or new tasks". Empty: off.
    pub progress_check: String,
    /// Skip a check-in that would fire more than this many minutes late (e.g. after sleep).
    pub late_checkin_minutes: u32,
    /// How much of the conversation each turn sees: recent exchanges and log entries.
    pub recent_turns: usize,
    pub recent_log_entries: usize,
    pub llm: LlmSettings,
    pub me: Me,
    pub sources: Sources,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schedule: Cron::parse("7,27,47 9-17 * * 1-5").expect("the default schedule parses"),
            work_hours: WorkHours::parse("9-17 Mon-Fri").expect("the default work hours parse"),
            miss_threshold: 3,
            remind_after_minutes: 5,
            checkin_popover: 0,
            notifier: Notifier::Desktop,
            archive_after_days: 1,
            log_keep_days: 30,
            report_at: ReportAt::End,
            report_lookback_days: 7,
            morning_bootstrap: DEFAULT_BOOTSTRAP.into(),
            progress_check: String::new(),
            late_checkin_minutes: 5,
            recent_turns: 6,
            recent_log_entries: 10,
            llm: LlmSettings::default(),
            me: Me::default(),
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

/// When the automatic end-of-day report runs. Written as "end", "off" or "HH:MM".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum ReportAt {
    /// At the end of work hours (17:00 when there are none).
    End,
    Off,
    At(NaiveTime),
}

impl TryFrom<String> for ReportAt {
    type Error = String;
    fn try_from(text: String) -> Result<Self, String> {
        match text.trim() {
            "end" => Ok(ReportAt::End),
            "off" | "" => Ok(ReportAt::Off),
            t => NaiveTime::parse_from_str(t, "%H:%M")
                .map(ReportAt::At)
                .map_err(|_| format!("report_at {t:?}: use \"end\", \"off\" or a time like \"17:30\"")),
        }
    }
}

/// Which backend runs the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum ProviderKind {
    /// The Claude API.
    #[serde(rename = "claude")]
    Claude,
    /// The Claude Code CLI (`claude -p`) on the person's Claude Code login.
    #[serde(rename = "claude-code", alias = "claude_code")]
    ClaudeCode,
    #[serde(rename = "ollama")]
    Ollama,
    #[serde(rename = "lmstudio")]
    LmStudio,
    /// Any other OpenAI-compatible server, at `base_url`.
    #[serde(rename = "openai", alias = "local", alias = "llamacpp")]
    OpenAi,
}

/// How hard the model thinks: `output_config.effort` for the API, `--effort` for Claude Code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

/// `checkin_popover`: a count, or true/false for 1/0.
fn popover_after<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Flag(bool),
        Count(u32),
    }
    Ok(match Either::deserialize(d).map_err(|_| serde::de::Error::custom("checkin_popover: use a number of check-ins, or true/false"))? {
        Either::Flag(on) => u32::from(on),
        Either::Count(n) => n,
    })
}

/// An effort setting, where "" means none (the model's default).
fn effort<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Effort>, D::Error> {
    let text = String::deserialize(d)?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    Effort::deserialize(serde::de::value::StrDeserializer::new(text.trim())).map(Some)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct LlmSettings {
    /// "claude" (the API), "claude-code" (shells out to `claude -p`), or "ollama" /
    /// "lmstudio" / "openai" for any OpenAI-compatible server.
    pub provider: ProviderKind,
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
    #[serde(deserialize_with = "effort")]
    pub light_effort: Option<Effort>,
    /// Building the list, long docs, design questions, reports.
    pub strong_model: String,
    #[serde(deserialize_with = "effort")]
    pub strong_effort: Option<Effort>,
    /// Words that send a message to the strong model.
    pub strong_keywords: Vec<String>,
    /// Messages at least this long go to the strong model.
    pub strong_min_chars: usize,
    /// Questions with more words than this go to the strong model.
    pub strong_question_words: usize,
    /// provider = "claude-code": tools Claude Code may use without asking. Add your own
    /// MCP servers here (e.g. "mcp__slack") to let the coach use them.
    pub claude_code_tools: Vec<String>,
    /// provider = "claude-code": load your own Claude Code MCP servers (Slack, Notion...)
    /// next to the coach's. Off, only the coach's tools are loaded.
    pub claude_code_user_mcp: bool,
}

impl Default for LlmSettings {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Claude,
            base_url: None,
            api_key_env: None,
            command: None,
            session_id: None,
            light_model: "claude-haiku-5-5".into(),
            light_effort: Some(Effort::Low),
            strong_model: "claude-opus-5-5".into(),
            strong_effort: Some(Effort::High),
            strong_keywords: [
                "explain", "why ", "how would", "how do we", "can we", "confused", "do we have enough",
            ]
            .map(String::from)
            .into(),
            strong_min_chars: 400,
            strong_question_words: 14,
            claude_code_tools: [
                "Bash", "Read", "Edit", "Write", "Glob", "Grep", "WebFetch", "WebSearch", "mcp__focus",
            ]
            .map(String::from)
            .into(),
            claude_code_user_mcp: true,
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

/// Who the person is, so the coach can tell their PRs, mentions and messages from other people's.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Me {
    pub name: Option<String>,
    /// Nicknames and handles people use for you.
    pub aliases: Vec<String>,
    pub github: Option<String>,
    /// Slack handle and/or member id, e.g. "@becker (U012345)".
    pub slack: Option<String>,
    pub email: Option<String>,
    /// Anything else the coach should know about you, in your own words.
    pub notes: String,
}

impl Me {
    /// The lines the coach sees each turn, or None when nothing is set.
    /// `slack_user_id` is the older `[sources]` setting, used when `slack` is unset.
    pub fn describe(&self, slack_user_id: Option<&str>) -> Option<String> {
        let mut out = String::new();
        let mut line = |label: &str, value: Option<&str>| {
            if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
                out.push_str(&format!("{label}: {v}\n"));
            }
        };
        line("Name", self.name.as_deref());
        line("Also goes by", Some(&self.aliases.join(", ")));
        line("GitHub", self.github.as_deref());
        line("Slack", self.slack.as_deref().or(slack_user_id));
        line("Email", self.email.as_deref());
        if !self.notes.trim().is_empty() {
            out.push_str(self.notes.trim());
            out.push('\n');
        }
        (!out.is_empty()).then_some(out)
    }
}

pub const DEFAULT_BOOTSTRAP: &str = "Morning: check my open PRs, review requests and PRs merged since yesterday (gh), carry over yesterday's open items, and update the list.";

/// Written on first run. Every setting is in it, so it doubles as the reference.
const DEFAULT_CONFIG: &str = include_str!("../../../docs/focus.example.toml");

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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> std::result::Result<Settings, String> {
        toml::from_str(toml).map_err(|e| e.to_string())
    }

    #[test]
    fn typed_settings_read_from_plain_text() {
        let s = parse(
            "schedule = \"*/20 9-17 * * 1-5\"\nwork_hours = \"9:30-17:30 Mon,Wed\"\nreport_at = \"17:15\"\n[llm]\nprovider = \"claude_code\"\nlight_effort = \"\"\nstrong_effort = \"max\"\n",
        )
        .unwrap();
        assert_eq!(s.schedule.as_str(), "*/20 9-17 * * 1-5");
        assert_eq!(s.work_hours.unwrap().to_string(), "9:30-17:30 Mon,Wed");
        assert_eq!(s.report_at, ReportAt::At(NaiveTime::from_hms_opt(17, 15, 0).unwrap()));
        assert_eq!(s.llm.provider, ProviderKind::ClaudeCode, "the old spelling still works");
        assert_eq!((s.llm.light_effort, s.llm.strong_effort), (None, Some(Effort::Max)));

        let s = parse("work_hours = \"\"\nreport_at = \"off\"\n").unwrap();
        assert_eq!((s.work_hours, s.report_at), (None, ReportAt::Off));
        assert_eq!(parse("checkin_popover = 2").unwrap().checkin_popover, 2);
        assert_eq!(parse("checkin_popover = true").unwrap().checkin_popover, 1, "true means every check-in");
        assert_eq!(parse("").unwrap().checkin_popover, 0);
        assert_eq!(parse("").unwrap().llm.provider, ProviderKind::Claude);
    }

    #[test]
    fn me_describes_only_what_is_set() {
        assert_eq!(parse("").unwrap().me.describe(None), None);
        let s = parse("[me]\nname = \"Sam\"\naliases = [\"sammy\", \"sk\"]\ngithub = \"skim\"\nnotes = \"I own billing.\\n\"\n").unwrap();
        assert_eq!(
            s.me.describe(Some("U1")).unwrap(),
            "Name: Sam\nAlso goes by: sammy, sk\nGitHub: skim\nSlack: U1\nI own billing.\n",
            "the old slack_user_id fills in for slack",
        );
    }

    #[test]
    fn the_example_config_sets_only_defaults() {
        let example = parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(format!("{example:?}"), format!("{:?}", Settings::default()));
    }

    /// Every field of the settings structs is mentioned in the example, set or commented.
    #[test]
    fn the_example_config_covers_every_setting() {
        let source = include_str!("config.rs");
        for name in ["Settings", "LlmSettings", "Sources", "Me"] {
            let body = source.split(&format!("pub struct {name} {{\n")).nth(1).unwrap().split("\n}").next().unwrap();
            for field in body.lines().filter_map(|l| l.trim().strip_prefix("pub ")?.split_once(':').map(|(f, _)| f)) {
                assert!(
                    DEFAULT_CONFIG.lines().any(|l| {
                        let l = l.trim_start_matches(['#', ' ']);
                        l.starts_with(&format!("{field} =")) || l.starts_with(&format!("[{field}]"))
                    }),
                    "docs/focus.example.toml doesn't mention {name}.{field}"
                );
            }
        }
    }

    #[test]
    fn mistakes_fail_when_the_config_loads() {
        for (toml, says) in [
            ("report_at = \"5pm\"", "report_at \"5pm\""),
            ("schedule = \"every 20 minutes\"", "cron needs 5 fields"),
            ("work_hours = \"nine to five\"", "expected e.g. \"9-17 Mon-Fri\""),
            ("[llm]\nprovider = \"gpt\"", "unknown variant `gpt`"),
            ("checkin_popover = \"often\"", "checkin_popover: use a number"),
            ("[llm]\nlight_effort = \"extreme\"", "unknown variant `extreme`"),
        ] {
            let err = parse(toml).unwrap_err();
            assert!(err.contains(says), "{toml}: {err}");
        }
    }
}

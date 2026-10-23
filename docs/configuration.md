# Configuration

Settings live in `~/.focus/focus.toml`. The first run writes [focus.example.toml](focus.example.toml): every setting with a comment saying what it does, the defaults set and the rest commented out. To see it before starting, run `focus tasks` once (that starts the daemon, which writes the file).

- Delete any line to fall back to its default.
- Edits apply within a couple of seconds, even while the daemon runs. A changed `schedule` replaces one the coach set.
- If the file stops parsing, the TUI and the menubar say why, and the previous settings stay until it's fixed.

The settings, by group (the example file has the details):

| Group | Settings |
|---|---|
| Check-ins | `schedule`, `work_hours`, `late_checkin_minutes`, `miss_threshold` |
| Notifications | `notifier`, `remind_after_minutes`, `checkin_popover` |
| What the coach runs on its own | `morning_bootstrap`, `progress_check`, `report_at` |
| How much it sees and keeps | `recent_turns`, `recent_log_entries`, `report_lookback_days`, `archive_after_days`, `log_keep_days` |
| `[llm]` | The provider and models. See [models](models.md). |
| `[sources]` | `github_org`, `repos_root`, `notion_pages` |
| `[me]` | Who you are: name, nicknames and handles, GitHub, Slack, email and notes, so the coach can tell your PRs and mentions from other people's |

## A workday config

```toml
schedule = "7,27,47 9-17 * * 1-5"
work_hours = "9-17 Mon-Fri"
report_at = "end"   # the report runs at 17:00

# Louder check-ins: a reminder after 5 minutes, and the popover once two go unanswered.
remind_after_minutes = 5
checkin_popover = 2

# Before the first message or check-in of the day:
morning_bootstrap = "Morning: check my open PRs, review requests and PRs merged since yesterday, carry over yesterday's open items, and update the list."

# Before every check-in:
progress_check = "check my PRs and Slack for progress on my current tasks or new tasks"

[llm]
provider = "claude-code"
light_model = "claude-haiku-5-5"
strong_model = "claude-opus-5-5"
claude_code_tools = ["Bash", "Read", "Glob", "Grep", "WebFetch", "WebSearch", "mcp__focus", "mcp__slack"]

[sources]
github_org = "my-org"
repos_root = "~/src"

[me]
name = "Sam Kim"
aliases = ["sam", "sk", "skim"]
github = "skim"
slack = "@sam (U012345)"
notes = "I'm on the billing team and own the invoices service. Reviews for api-gateway are mine too."
```

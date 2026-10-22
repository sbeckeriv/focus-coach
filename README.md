# Focus Coach

A local app that keeps one person on track during the workday. It keeps a task list, checks in on a timer, and uses an LLM to read your replies and keep the list current. It can also look things up for you, draft messages, and write an end-of-day report.

## Layout

| Crate | What it is |
|---|---|
| `crates/core` (`focus-core`) | Store, log, scheduler, providers, interpreter and tools, the MCP tool server, and the daemon with its socket protocol |
| `crates/focus` | The `focus` CLI and terminal UI |
| `crates/menubar` | The macOS menubar app (Tauri v2), with a popover chat pane |

One daemon owns the single check-in schedule. Every front end talks to it over `~/.focus/focus.sock`. The CLI starts the daemon on demand. The menubar app hosts the daemon itself when none is running.

## Setup

1. **Prerequisites:** Rust 1.88 or later, and the GitHub CLI signed in (`gh auth login`) for PR lookups.

2. **Install the CLI:**

   ```sh
   cargo install --locked --path crates/focus   # puts `focus` in ~/.cargo/bin
   ```

3. **Pick a model.** See [Models](#models) for details. The options are:
   - **The Claude API** (the default): `export ANTHROPIC_API_KEY=sk-ant-...`, or `ant auth login`.
   - **Claude Code**, on your Claude Code login with no API key: set `provider = "claude-code"`.
   - **A local model** through Ollama or LM Studio.

4. **Configure.** The first run writes `~/.focus/focus.toml` with defaults. To review it before starting, run `focus tasks` once (that starts the daemon, which writes the file), then edit:
   - `schedule`: when check-ins fire, as cron (default minutes 7, 27 and 47 of each hour).
   - `work_hours`: the hours check-ins may fire in (default `"9-17 Mon-Fri"`; `"9:30-17:30 Mon,Wed,Fri"` also works; `""` means any time). The workday's end is also when the report runs.
   - `morning_bootstrap`: what runs before your first message or check-in each day. The default checks your PRs, review requests and recently merged PRs with `gh`. `""` turns it off.
   - `progress_check`: what runs right before each check-in, to tick off finished work and add new work from your sources. Off when blank (the default). See [Examples](#examples).
   - `report_at`: the automatic end-of-day report, at `"end"` of work hours (the default), a time like `"17:30"`, or `"off"`.
   - `notifier`: `"desktop"` for macOS notifications, or `"terminal"`.
   - `archive_after_days`: how long done items stay on the list before moving to `done.md` (default 1; 0 moves them right away).
   - `log_keep_days`: how much of `log.md` to keep (default 30; 0 keeps everything).
   - `recent_turns`, `recent_log_entries`, `report_lookback_days`, `late_checkin_minutes`: how much each turn and the report see, and how late a check-in may fire after the machine sleeps.
   - `[llm]`: the provider and models.
   - `[sources]`: `github_org` to scope PR searches, and `repos_root` so the coach can read your local code.

   If the daemon was already running when you edited the file, restart it with `focus stop` and then any command.

5. **Start the day:**

   ```sh
   focus start "look at my PRs and make a task list"
   To start the daemon at login without the menubar app, run `focus autostart on` (`off` removes it). It installs a launch agent with your current PATH, so `gh` and `claude` are found.

   ```

6. **Optional: the menubar app.**

   ```sh
   cargo install --locked tauri-cli --version "^2"
   cd crates/menubar && cargo tauri build
   ```

   This builds `Focus Coach.app` under `target/release/bundle/macos/`. Drag it to Applications, then turn on **Start at login** in its menu to keep it always on. While it runs, check-ins arrive as its own notifications; clicking one opens the popover. The window resizes, and the bar between the tasks and the conversation drags; click a task's title for its notes and links, and to add a note. For a quick try without bundling, run `cargo run -p focus-menubar`. CI also builds the app; see [CI](#ci).

   Apps launched from Finder don't see your shell's environment variables, such as `ANTHROPIC_API_KEY`. With the menubar app, use `ant auth login`, the `claude-code` provider, or a local model.

## Use

```sh
focus start "look at my PRs and make a task list"   # build the list from sources and start check-ins
focus                                              # terminal UI
focus say "yup. trying option b."                  # reply to a check-in, ask anything
focus say "add a task: renew passport, due friday"
focus add "renew passport" --due fri              # add a task instantly (--section, --due)
focus add https://github.com/org/repo/pull/12      # fetch a link and file it, through the coach
focus note t3 "waiting on legal"                   # add context to a task (notes accumulate)
focus approve a1 | reject a1                       # outgoing actions the coach proposed
focus tasks                                        # the list; --markdown prints tasks.md as stored
focus checkin                                      # check in now (after the progress check)
focus next | pause | resume | report | status | stop
focus autostart on | off                           # start the daemon at login
```

Nothing goes out without your approval. When the coach wants to send a message, post a PR comment, save a draft or commit, it proposes the action with the exact text and an id like `a1`, then waits. `focus approve a1`, `/approve a1` in the TUI, or the Approve button in the menubar runs one more turn that does exactly that. `focus status` lists what's waiting. Notes to self ("remind me at 3") need no approval.

In the terminal UI, type to talk to the coach. Slash commands map to the CLI: `/next`, `/pause`, `/resume`, `/report`, `/add <title or url>` (with `--due` and `--section`), `/note [id] <text>` (the selected task when there's no id), `/approve <id>`, `/reject <id>`, `/copy` and `/start [sources]`. `/quit`, `/exit`, or `exit` on its own closes it. Alt+Enter adds a line break, and pasted text keeps its lines. With the mouse:
- **Click a task's mark** (○ ▶ ! ✓) to check it off or reopen it. This writes straight to the list, with no model call.
- **Click a task's title** to see its notes and links. Then press Enter on an empty input to open its first link.
- **Drag the border** between the panes to resize them (or press Alt+←/→).
- **Scroll** either pane with the wheel; PageUp and PageDown scroll the conversation. Long titles and long input wrap.

To copy text, press **F2** for select mode: the mouse goes back to your terminal, so drag to select and copy as usual, then F2 again. **Ctrl+Y** (or `/copy`) copies the coach's last reply straight to the clipboard.

Everything lives in `~/.focus` (override with `FOCUS_HOME`):
- `tasks.md`: the task list. You can edit it by hand at any time. Items without an `{id=..}` get one on the next save.
- `done.md`: finished tasks, moved out of `tasks.md` after `archive_after_days` and grouped by day, newest first.
- `log.md`: the check-in log, trimmed to `log_keep_days`.
- `focus.toml`: settings, written with defaults on first run.
- `state.json`: pause state, the schedule, actions awaiting approval, and the current Claude Code session.
- `reports/`: end-of-day reports.

## Examples

### A workday config

```toml
schedule = "7,27,47 9-17 * * 1-5"
work_hours = "9-17 Mon-Fri"
report_at = "end"   # the report runs at 17:00

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
```

### The progress check

With `progress_check` set, the coach looks at your sources just before each check-in. It checks off a task only when a source clearly shows it's done, such as a merged PR. It adds a task only when a source says it's yours. Anything less certain is mentioned, not changed. What it found rides along with the check-in:

```
⏰ What are you on? Top of the list: Review billing docs (t2).
lodash PRs merged, so I checked off t1. New review request on api#88: added t9.
```

When nothing changed, you get the plain check-in. Leave `progress_check` blank to skip it. It runs once per check-in (every 20 minutes by default), so it uses more of your model plan than check-ins alone. Run `focus checkin` to try it now.

### Slack, Notion and other MCP servers (claude-code)

Claude Code's own MCP servers load into every turn, but a check-in runs with nobody around to click "allow". So the coach can only use tools listed in `claude_code_tools`. List each server as `mcp__<name>`, with the name from `claude mcp list`:

```sh
claude mcp list
# slack: npx -y @modelcontextprotocol/server-slack - ✔ Connected
# claude.ai Notion: https://mcp.notion.com/mcp - ✔ Connected
```

```toml
claude_code_tools = ["Bash", "Read", "Glob", "Grep", "WebFetch", "WebSearch",
                     "mcp__focus", "mcp__slack", "mcp__claude_ai_Notion"]
```

- A claude.ai connector named `claude.ai Notion` becomes `mcp__claude_ai_Notion`.
- `mcp__focus` (the coach's own tools) must stay in the list.
- Setting `claude_code_tools` replaces the default list, so include the built-ins you want.
- A server that isn't listed is refused without an error, and the coach acts as if it isn't connected.

### Adding tasks

```sh
focus add "renew passport" --due fri --section Personal   # instant, no model call
focus add https://github.com/my-org/api/pull/88           # the coach fetches it and writes the title
focus say "add a task: draft the Q4 plan, due end of month" # the coach picks the section and date
```

`--due` takes `2026-11-02`, `today`, `tomorrow` or a weekday (`fri` means the next Friday). In the TUI it's `/add renew passport --due fri`.

### Approving outgoing actions

```
$ focus say "tell them on api#88 that it looks good once CI passes"
┌─ a1 pr comment to https://github.com/my-org/api/pull/88 (awaiting approval)
│ Looks good to me once CI is green. Thanks!
└─ focus approve a1 · focus reject a1
Proposed a1, a comment on api#88. Approve it to post.

$ focus approve a1
  · a1: commented on https://github.com/my-org/api/pull/88
Posted the comment on api#88.
```

`focus status` lists anything still waiting. In the menubar app, the proposal has Approve and Reject buttons.

### Replying to check-ins

```
⏰ What are you on? Top of the list: Review billing docs (t2).
$ focus say "yup. trying option b."      # logged, no questions back
$ focus say "on a prod incident"         # off-list: it asks once, add it or get back to t2?
$ focus say "billing docs done, half of the lodash PRs cut"
  · done t2: Review billing docs
  · done t1: Cut half of the lodash PRs; left as t10: Cut the rest of the lodash PRs
$ focus say "quiet until tomorrow"       # pauses check-ins until your next message
```

## Models

Light-model turns handle check-ins and short replies. Strong-model turns handle building the list, links, design questions and the report.

**The Claude API** is the default, with `claude-haiku-5-5` as the light model and `claude-opus-5-5` as the strong one. The key comes from `ANTHROPIC_API_KEY`, then `ANTHROPIC_AUTH_TOKEN`, then `ant auth login`.

**Claude Code** runs each turn through `claude -p` on your Claude Code login. Usage counts against your Claude Code plan instead of API credits.

```toml
[llm]
provider = "claude-code"
light_model = "claude-haiku-5-5"
strong_model = "claude-opus-5-5"
# command = "/path/to/claude"   # if it isn't on PATH or in ~/.local/bin
# session_id = "<uuid>"         # resume one session forever; unset = a new session each day
```

- Claude Code runs its own tool loop. Its built-in tools (Bash, Read, Edit, Write, Glob, Grep, WebFetch, WebSearch) are allowed without prompts. The coach's own tools come from `focus mcp`, a small MCP server that forwards each call to the daemon. Because it runs unattended with Bash, every check-in reply can run commands on your machine.
- Turns resume one Claude Code session: a new one each day, or the `session_id` you set. `focus status` prints it. Open it with `claude --resume <id>` from the folder it runs in to see every tool call the coach made.
- Claude Code runs in `repos_root` if that's set, or in `~/.focus` otherwise. Your own Claude Code settings, hooks and CLAUDE.md apply.
- Your own Claude Code MCP servers load too (`claude_code_user_mcp = true`, the default), so Slack, Notion or Jira work through them if you have them. List their tools in `claude_code_tools`, for example `"mcp__slack"`, so they run without a prompt. That list also replaces the default built-ins, so you can drop `Bash` or `Write` there.
- Approval is enforced for the coach's own tools. Under Claude Code, Bash and your MCP tools could still send something without asking, and only the prompt rule stops them. Leave them out of `claude_code_tools` if that matters to you.

**Local models** work through any OpenAI-compatible server with tool calling:

```toml
[llm]
provider = "lmstudio"      # localhost:1234; or "ollama" (localhost:11434), or "openai" with base_url
light_model = "google/gemma-4-26b-a4b"
strong_model = "google/gemma-4-26b-a4b"
```

Give the model at least 32k tokens of context. A morning bootstrap that fetches several PRs needs it.

## Tests

`cargo test --workspace` runs offline and spends nothing:
- The integration tests run the real daemon, CLI and menubar request path against a scripted mock model server (`crates/core/tests/support`).
- The Claude Code provider is tested with a fake `claude` script that plays Claude Code's side of MCP (`crates/focus/tests/cli.rs`).

## CI

GitHub Actions has two workflows:
- **`ci.yml`** runs clippy and the tests on every push and pull request.
- **`app.yml`** builds `Focus Coach.app` for Apple Silicon on pushes to `main` and on `v*` tags, and uploads it as a workflow artifact. A `v*` tag also attaches the zip to a GitHub release.

The app isn't signed or notarized, so macOS blocks it on first launch. Clear the quarantine flag after unzipping:

```sh
xattr -dr com.apple.quarantine "Focus Coach.app"
```

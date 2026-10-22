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
   cargo install --path crates/focus     # puts `focus` in ~/.cargo/bin
   ```

3. **Pick a model.** See [Models](#models) for details. The options are:
   - **The Claude API** (the default): `export ANTHROPIC_API_KEY=sk-ant-...`, or `ant auth login`.
   - **Claude Code**, on your Claude Code login with no API key: set `provider = "claude-code"`.
   - **A local model** through Ollama or LM Studio.

4. **Configure.** The first run writes `~/.focus/focus.toml` with defaults. To review it before starting, run `focus tasks` once (that starts the daemon, which writes the file), then edit:
   - `schedule` and `work_hours`: when check-ins fire (default every 20 minutes, 9–17 on weekdays).
   - `notifier`: `"desktop"` for macOS notifications, or `"terminal"`.
   - `archive_after_days`: how long done items stay on the list before moving to `done.md` (default 1; 0 moves them right away).
   - `[llm]`: the provider and models.
   - `[sources]`: `github_org` to scope PR searches, and `repos_root` so the coach can read your local code.

   If the daemon was already running when you edited the file, restart it with `focus stop` and then any command.

5. **Start the day:**

   ```sh
   focus start "look at my PRs and make a task list"
   ```

6. **Optional: the menubar app.**

   ```sh
   cargo install tauri-cli --version "^2"
   cd crates/menubar && cargo tauri build
   ```

   This builds `Focus Coach.app` under `target/release/bundle/macos/`. Drag it to Applications, and add it under System Settings → General → Login Items to keep it always on. For a quick try without bundling, run `cargo run -p focus-menubar`. CI also builds the app; see [CI](#ci).

   Apps launched from Finder don't see your shell's environment variables, such as `ANTHROPIC_API_KEY`. With the menubar app, use `ant auth login`, the `claude-code` provider, or a local model.

## Use

```sh
focus start "look at my PRs and make a task list"   # build the list from sources and start check-ins
focus                                              # terminal UI
focus say "yup. trying option b."                  # reply to a check-in, ask anything
focus say "add a task: renew passport, due friday"
focus add https://github.com/org/repo/pull/12      # fetch a link and file it
focus tasks                                        # the list; --markdown prints tasks.md as stored
focus next | pause | resume | report | status | stop
```

In the terminal UI, type to talk to the coach. Slash commands map to the CLI: `/next`, `/pause`, `/resume`, `/report`, `/add <url>`, `/start [sources]` and `/quit`. With the mouse:
- **Click a task's mark** (○ ▶ ! ✓) to check it off or reopen it. This writes straight to the list, with no model call.
- **Click a task's title** to see its notes and links. Then press Enter on an empty input to open its first link.
- **Scroll** the conversation with the wheel or PageUp and PageDown.

Hold Option (or Shift, in some terminals) to select text.

Everything lives in `~/.focus` (override with `FOCUS_HOME`):
- `tasks.md`: the task list. You can edit it by hand at any time. Items without an `{id=..}` get one on the next save.
- `done.md`: finished tasks, moved out of `tasks.md` after `archive_after_days` and grouped by day, newest first.
- `log.md`: the check-in log.
- `focus.toml`: settings, written with defaults on first run.
- `state.json`: pause state, the schedule, and the current Claude Code session.
- `reports/`: end-of-day reports.

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

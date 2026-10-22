# Focus Coach

A local app that keeps one person on track during the workday. It keeps a task list, checks in on a timer, and uses an LLM to read replies and keep the list current. See `spec.md`.

## Layout

| Crate | What it is |
|---|---|
| `crates/core` (`focus-core`) | Store, log, scheduler, providers, interpreter and tools, and the daemon with its socket protocol |
| `crates/focus` | The `focus` CLI and terminal UI |
| `crates/menubar` | The macOS menubar app (Tauri v2), with a popover chat pane |

One daemon owns the single check-in schedule. Every front end talks to it over `~/.focus/focus.sock`. The CLI starts the daemon on demand. The menubar app hosts the daemon itself when none is running.

## Setup

1. **Prerequisites:** Rust 1.88 or later, and the GitHub CLI signed in (`gh auth login`) for PR lookups.

2. **Install the CLI:**

   ```sh
   cargo install --path crates/focus     # puts `focus` in ~/.cargo/bin
   ```

3. **Pick a model.** For Claude, export a key or sign in with the `ant` CLI:

   ```sh
   export ANTHROPIC_API_KEY=sk-ant-...   # or: ant auth login
   ```

   For a local model, see [Models](#models) below.

4. **Configure.** The first run writes `~/.focus/focus.toml` with defaults. To review it before starting, run `focus tasks` once (that starts the daemon, which writes the file), then edit:
   - `schedule` and `work_hours`: when check-ins fire (default every 20 minutes, 9–17 on weekdays).
   - `notifier`: `"desktop"` for macOS notifications, or `"terminal"`.
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

   This builds `Focus Coach.app` under `target/release/bundle/macos/`. Drag it to Applications, and add it under System Settings → General → Login Items to keep it always on. For a quick try without bundling, run `cargo run -p focus-menubar`.

   The app reads its key from its own environment. Apps launched from Finder don't see your shell's `ANTHROPIC_API_KEY`, so use `ant auth login` or a local model with the menubar app.

## Use

```sh
focus start "look at my PRs and make a task list"   # bootstrap + start check-ins
focus                                              # terminal UI
focus say "yup. trying option b."                   # reply to a check-in
focus add https://github.com/org/repo/pull/12
focus next | pause | resume | report | status | tasks | stop
cargo tauri build -- -p focus-menubar              # or: cargo run -p focus-menubar
```

Everything lives in `~/.focus` (override with `FOCUS_HOME`):
- `tasks.md`: the task list. You can edit it by hand at any time.
- `log.md`: the check-in log.
- `focus.toml`: settings, written with defaults on first run.
- `reports/`: end-of-day reports.

## Models

Claude is the default. The light model is `claude-haiku-5-5` and the strong one is `claude-opus-5-5`. The key comes from `ANTHROPIC_API_KEY`, or from `ant auth login`.

To run on your Claude Code login instead of an API key, set `provider = "claude-code"`. Each turn then runs `claude -p` with Claude Code's built-in tools (Bash, Read, Edit, Write, WebFetch and more, allowed without prompts), plus the coach's own tools served by `focus mcp`. Claude Code runs in `repos_root` if that's set, or in `~/.focus` otherwise. Set `command` under `[llm]` if `claude` isn't on PATH or in `~/.local/bin`. Usage counts against your Claude Code plan.

Local models work through any OpenAI-compatible server with tool calling, for example:

```toml
[llm]
provider = "ollama"        # or "lmstudio" (localhost:1234), or "openai" with base_url
light_model = "qwen3:8b"
strong_model = "qwen3:32b"
```

## Tests

`cargo test --workspace` runs offline. The integration tests run the real daemon, CLI and menubar request path against a scripted mock model server (`crates/core/tests/support`).

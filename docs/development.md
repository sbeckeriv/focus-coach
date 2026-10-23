# Development

## Layout

| Crate | What it is |
|---|---|
| `crates/core` (`focus-core`) | Store, log, scheduler, providers, interpreter and tools, the MCP tool server, and the daemon with its socket protocol |
| `crates/focus` | The `focus` CLI and terminal UI |
| `crates/menubar` | The macOS menubar app (Tauri v2), with a popover chat pane |

One daemon owns the single check-in schedule. Every front end talks to it over `~/.focus/focus.sock`. The CLI starts the daemon on demand. The menubar app hosts the daemon itself when none is running.

The default `focus.toml` is [docs/focus.example.toml](focus.example.toml), compiled in. A test checks that it mentions every setting and sets only defaults, so a new setting needs a line there.

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

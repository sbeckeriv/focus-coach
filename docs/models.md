# Models

Light-model turns handle check-ins and short replies. Strong-model turns handle building the list, links, design questions and the report. What sends a message to the strong model is configurable; see `strong_keywords` in [the example config](focus.example.toml).

## The Claude API

The default, with `claude-haiku-5-5` as the light model and `claude-opus-5-5` as the strong one. The key comes from `ANTHROPIC_API_KEY`, then `ANTHROPIC_AUTH_TOKEN`, then `ant auth login`.

## Claude Code

Runs each turn through `claude -p` on your Claude Code login. Usage counts against your Claude Code plan instead of API credits.

```toml
[llm]
provider = "claude-code"
light_model = "claude-haiku-5-5"
strong_model = "claude-opus-5-5"
# command = "/path/to/claude"   # if it isn't on PATH or in ~/.local/bin
# session_id = "<uuid>"         # resume one session forever
```

- Claude Code runs its own tool loop. Its built-in tools (Bash, Read, Edit, Write, Glob, Grep, WebFetch, WebSearch) are allowed without prompts. The coach's own tools come from `focus mcp`, a small MCP server that forwards each call to the daemon. Because it runs unattended with Bash, every check-in reply can run commands on your machine.
- Turns resume one Claude Code session: a new one each day, whenever an update changes the coach's rules, on `focus new-session` (`/new-session` in the TUI or menubar), or the `session_id` you set. `focus status` prints it. Open it with `claude --resume <id>` from the folder it runs in to see every tool call the coach made.
- Claude Code runs in `repos_root` if that's set, or in `~/.focus` otherwise. Your own Claude Code settings, hooks and CLAUDE.md apply.
- Your own Claude Code MCP servers load too (`claude_code_user_mcp = true`, the default), so Slack, Notion or Jira work through them if you have them. List their tools in `claude_code_tools`, for example `"mcp__slack"`, so they run without a prompt. That list also replaces the default built-ins, so you can drop `Bash` or `Write` there. See [the guide](guides.md#slack-notion-and-other-mcp-servers-claude-code).
- Approval is enforced for the coach's own tools. Under Claude Code, Bash and your MCP tools could still send something without asking, and only the prompt rule stops them. Leave them out of `claude_code_tools` if that matters to you.
- When the menubar app hosts the daemon, macOS may ask whether "focus-menubar" may find devices on your local network. That's Claude Code or one of your MCP servers (Home Assistant, say) reaching your network. The coach itself doesn't need it, and model servers on localhost work either way.

## Local models

Any OpenAI-compatible server with tool calling:

```toml
[llm]
provider = "lmstudio"      # localhost:1234; or "ollama" (localhost:11434), or "openai" with base_url
light_model = "google/gemma-4-26b-a4b"
strong_model = "google/gemma-4-26b-a4b"
```

Give the model at least 32k tokens of context. A morning bootstrap that fetches several PRs needs it.

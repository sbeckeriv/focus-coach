# Guides

## Replying to check-ins

```
⏰ What are you on? Top of the list: Review billing docs (t2).
$ focus say "yup. trying option b."      # logged, no questions back
$ focus say "on a prod incident"         # off-list: it adds it and marks it as what you're on
$ focus say "started the security fix, top priority"  # same, at priority 1
$ focus say "might look at the flaky test" # unclear: it asks once, add it or get back to t2?
$ focus say "billing docs done, half of the lodash PRs cut"
  · done t2: Review billing docs
  · done t1: Cut half of the lodash PRs; left as t10: Cut the rest of the lodash PRs
$ focus say "quiet until tomorrow"       # pauses check-ins until your next message
```

## Working on more than one thing

The top of the list is just the most important open item. What you're actually on is separate: mark any number of tasks **active** and check-ins ask about them instead ("Still Deploy (t3) and Review (t5)?"). Tell the coach ("on the deploy and the review", "switched off the review"), run `focus on t3 t5` / `focus off t5`, use `/on` and `/off` or right-click in the TUI, or hover a task in the menubar and press **On it**. A task stays active until you turn it off; in `tasks.md` it's a bare `active` in the `{..}`, so you can edit it by hand too. With one active task the check-in notification offers **Done** for it; with several, it offers **Still on it**, and you check things off in the popover.

## The progress check

With `progress_check` set, the coach looks at your sources just before each check-in. It checks off a task only when a source clearly shows it's done, such as a merged PR. It adds a task only when a source says it's yours. Anything less certain is mentioned, not changed. What it found rides along with the check-in:

```
⏰ What are you on? Top of the list: Review billing docs (t2).
lodash PRs merged, so I checked off t1. New review request on api#88: added t9.
```

When nothing changed, you get the plain check-in. Leave `progress_check` blank to skip it. It runs once per check-in (every 20 minutes by default), so it uses more of your model plan than check-ins alone. Run `focus checkin` to try it now.

## Slack, Notion and other MCP servers (claude-code)

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

## Adding tasks

```sh
focus add "renew passport" --due fri --section Personal   # instant, no model call
focus add https://github.com/my-org/api/pull/88           # the coach fetches it and writes the title
focus say "add a task: draft the Q4 plan, due end of month" # the coach picks the section and date
```

`--due` takes `2026-11-02`, `today`, `tomorrow` or a weekday (`fri` means the next Friday). In the TUI it's `/add renew passport --due fri`.

## Approving outgoing actions

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

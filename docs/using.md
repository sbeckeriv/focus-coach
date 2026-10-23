# Using Focus Coach

## The CLI

```sh
focus start "look at my PRs and make a task list"   # build the list from sources and start check-ins
focus                                              # terminal UI
focus say "yup. trying option b."                  # reply to a check-in, ask anything
focus say "add a task: renew passport, due friday"
focus add "renew passport" --due fri              # add a task instantly (--section, --due)
focus add https://github.com/org/repo/pull/12      # fetch a link and file it, through the coach
focus note t3 "waiting on legal"                   # add context to a task (notes accumulate)
focus on t3 t5 | off t3                            # mark what you're on now; several can be active
focus approve a1 | reject a1                       # outgoing actions the coach proposed
focus tasks                                        # the list; --markdown prints tasks.md as stored
focus checkin                                      # check in now (after the progress check)
focus next | pause | resume | report | status | stop
focus new-session                                  # the coach forgets the conversation so far; the list stays
focus autostart on | off                           # start the daemon at login
```

One daemon owns the check-in schedule, and the CLI starts it on demand. To start it at login without the menubar app, run `focus autostart on` (`off` removes it). It installs a launch agent with your current PATH, so `gh` and `claude` are found.

## The terminal UI

Type to talk to the coach. Slash commands map to the CLI: `/next`, `/pause`, `/resume`, `/report`, `/new-session`, `/add <title or url>` (with `--due` and `--section`), `/note [id] <text>` (the selected task when there's no id), `/on [ids]` and `/off [ids]` (likewise), `/approve <id>`, `/reject <id>`, `/copy` and `/start [sources]`. `/quit`, `/exit`, or `exit` on its own closes it. Alt+Enter adds a line break, and pasted text keeps its lines. With the mouse:
- **Click a task's mark** (○ ▶ ● ! ✓) to check it off or reopen it. This writes straight to the list, with no model call.
- **Click a task's title** to see its notes and links. Then press Enter on an empty input to open its first link.
- **Right-click a task** to mark it as what you're on (●), or to unmark it.
- **Click a link** in the conversation to open it in your browser.
- **Drag the border** between the panes to resize them (or press Alt+←/→).
- **Scroll** either pane with the wheel; PageUp and PageDown scroll the conversation. Long titles and long input wrap.

To copy text, press **F2** for select mode: the mouse goes back to your terminal, so drag to select and copy as usual, then F2 again. **Ctrl+Y** (or `/copy`) copies the coach's last reply straight to the clipboard.

## The menubar app

Click the icon for the popover: the task list on top, the conversation below, and the same slash commands as the TUI. The header shows when the next check-in is, and what you're on. Hover a task and press **On it** to mark it active; click its title for its notes and links, and to add a note. The window resizes, and the bar between the tasks and the conversation drags. Messages show the time they were sent, and links open in your browser. For a quick try without bundling, run `cargo run -p focus-menubar`.

The app hosts the daemon itself when none is running.

### Notifications

While the app runs, check-ins arrive as its own notifications:
- They stay on screen until you answer or close them. To change that, go to System Settings → Notifications → Focus Coach and pick banners (slide away) or persistent (stay). A Mac that ran an older build keeps the style it had until you change it there.
- Clicking one opens the popover. Its **Still on it** and **Done** buttons answer about what you're on (or the top task) without opening it. **Done** is only offered when that's a single task.
- While a check-in waits on your reply, the menubar icon turns red.
- An unanswered check-in comes back as a reminder with a sound (`remind_after_minutes`), and the popover can open on its own after several go unanswered (`checkin_popover`). See [configuration](configuration.md).
- A Focus mode or Do Not Disturb holds them in Notification Center. Add Focus Coach to that Focus's allowed apps to let them through.

## Approving outgoing actions

Nothing goes out without your approval. When the coach wants to send a message, post a PR comment, save a draft or commit, it proposes the action with the exact text and an id like `a1`, then waits. `focus approve a1`, `/approve a1` in the TUI, or the Approve button in the menubar runs one more turn that does exactly that. `focus status` lists what's waiting. Notes to self ("remind me at 3") need no approval. See [the guide](guides.md#approving-outgoing-actions) for an example.

## Files

Everything lives in `~/.focus` (override with `FOCUS_HOME`):
- `tasks.md`: the task list. You can edit it by hand at any time. Items without an `{id=..}` get one on the next save.
- `done.md`: finished tasks, moved out of `tasks.md` after `archive_after_days` and grouped by day, newest first.
- `log.md`: the check-in log, trimmed to `log_keep_days`.
- `focus.toml`: settings. See [configuration](configuration.md).
- `state.json`: pause state, the schedule, actions awaiting approval, and the current Claude Code session.
- `reports/`: end-of-day reports.

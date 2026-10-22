mod list;
mod tui;

use std::io::IsTerminal;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use focus_core::{Event, Paths, Request, client, daemon};

/// Focus Coach: a task list, timed check-ins, and an LLM that keeps them current.
#[derive(Parser)]
#[command(name = "focus", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start check-ins (replacing any schedule), optionally building the list from sources.
    Start {
        /// e.g. "look at my PRs and https://notion.so/... and make a task list"
        text: Vec<String>,
    },
    /// Pause check-ins until your next message.
    Pause,
    /// Resume check-ins and summarize where things stand.
    Resume,
    /// Fetch a link, summarize it and file it on the list.
    Add { url: String },
    /// The next item, with a first step.
    Next,
    /// Pause and write the end-of-day report.
    Report,
    /// Send anything: a check-in reply, a question, a command.
    Say { text: Vec<String> },
    /// Show the check-in state.
    Status,
    /// Print the task list.
    Tasks {
        /// Print tasks.md as stored, with every field.
        #[arg(long)]
        markdown: bool,
    },
    /// Open the terminal UI (the default with no command).
    Tui,
    /// Run the daemon in the foreground.
    Daemon,
    /// Stop the daemon.
    Stop,
    /// Serve the coach's tools over MCP to a claude-code turn (started by the daemon).
    #[command(hide = true)]
    Mcp { turn: u64 },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::from_env()?;
    let command = cli.command.unwrap_or(Command::Tui);
    let mut raw_tasks = false;

    let request = match command {
        Command::Daemon => return daemon::run(paths).await,
        Command::Mcp { turn } => return focus_core::mcp::serve_stdio(paths, turn).await,
        Command::Tui => {
            ensure_daemon(&paths).await?;
            return tui::run(paths).await;
        }
        Command::Status | Command::Stop if !client::is_running(&paths).await => {
            println!("The focus daemon isn't running. Start it with `focus start`.");
            return Ok(());
        }
        Command::Start { text } => Request::Start { text: joined(text) },
        Command::Pause => Request::Pause,
        Command::Resume => Request::Resume,
        Command::Add { url } => Request::Add { url },
        Command::Next => Request::Next,
        Command::Report => Request::Report,
        Command::Say { text } => match joined(text) {
            Some(text) => Request::Say { text },
            None => bail!("say what? e.g. focus say \"on the lodash PRs\""),
        },
        Command::Status => Request::Status,
        Command::Tasks { markdown } => {
            raw_tasks = markdown;
            Request::Tasks
        }
        Command::Stop => Request::Shutdown,
    };
    ensure_daemon(&paths).await?;
    let mut failed = false;
    client::request(&paths, request, |event| {
        failed |= matches!(event, Event::Error { .. });
        match &event {
            Event::Tasks { list } if !raw_tasks => {
                let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
                print!("{}", list::format(list, chrono::Local::now().date_naive(), color));
            }
            _ => print_event(&event),
        }
    })
    .await?;
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn joined(words: Vec<String>) -> Option<String> {
    let text = words.join(" ");
    (!text.trim().is_empty()).then_some(text)
}

/// Starts `focus daemon` in the background if nothing is listening yet.
async fn ensure_daemon(paths: &Paths) -> Result<()> {
    if client::is_running(paths).await {
        return Ok(());
    }
    paths.ensure()?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths.daemon_log())?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("daemon").stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()?;
    for _ in 0..50 {
        if client::is_running(paths).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("the daemon didn't start; see {}", paths.daemon_log().display())
}

fn print_event(event: &Event) {
    match event {
        Event::CheckIn { text } => println!("⏰ {text}"),
        Event::Reply { text, .. } | Event::Ok { text } => println!("{text}"),
        Event::Question { text, .. } => println!("? {text}"),
        Event::Preview { kind, content, .. } => {
            println!("┌─ {kind} (preview, not sent)");
            for line in content.lines() {
                println!("│ {line}");
            }
            println!("└─");
        }
        Event::Changed { summary, .. } => println!("  · {summary}"),
        Event::Report { text, path, .. } => println!("{text}\n\nSaved to {path}"),
        Event::Error { text, .. } => eprintln!("error: {text}"),
        Event::Status { status } => {
            println!("{}", status.summary());
            if let Some(id) = &status.session {
                println!("Claude Code session {id} (open it with: claude --resume {id})");
            }
        }
        Event::Tasks { list } => print!("{}", list.render()),
        Event::TurnDone { .. } | Event::ToolResult { .. } | Event::Done => {}
    }
}

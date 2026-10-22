use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::config::Paths;
use crate::protocol::{Event, Request};

pub async fn is_running(paths: &Paths) -> bool {
    UnixStream::connect(paths.socket()).await.is_ok()
}

struct Conn {
    lines: Lines<BufReader<OwnedReadHalf>>,
    _write: OwnedWriteHalf,
}

async fn send(paths: &Paths, req: &Request) -> Result<Conn> {
    let stream = UnixStream::connect(paths.socket())
        .await
        .context("the focus daemon isn't running (start it with `focus start`)")?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    Ok(Conn { lines: BufReader::new(read).lines(), _write: write })
}

/// Sends one request and hands each event to `on_event` until the daemon says `Done`.
pub async fn request(paths: &Paths, req: Request, mut on_event: impl FnMut(Event)) -> Result<()> {
    let mut conn = send(paths, &req).await?;
    while let Some(line) = conn.lines.next_line().await? {
        match serde_json::from_str::<Event>(&line)? {
            Event::Done => return Ok(()),
            ev => on_event(ev),
        }
    }
    anyhow::bail!("the daemon closed the connection early")
}

pub struct Subscription {
    conn: Conn,
}

impl Subscription {
    /// The next broadcast event, or `None` once the daemon goes away.
    pub async fn next(&mut self) -> Result<Option<Event>> {
        match self.conn.lines.next_line().await? {
            Some(line) => Ok(Some(serde_json::from_str(&line)?)),
            None => Ok(None),
        }
    }
}

pub async fn subscribe(paths: &Paths) -> Result<Subscription> {
    Ok(Subscription { conn: send(paths, &Request::Subscribe { notifies: false }).await? })
}

/// Subscribes as the client that shows check-in notifications, so the daemon skips its own.
pub async fn subscribe_as_notifier(paths: &Paths) -> Result<Subscription> {
    Ok(Subscription { conn: send(paths, &Request::Subscribe { notifies: true }).await? })
}

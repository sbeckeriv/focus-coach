//! Menubar front end. Hosts the daemon in-process when none is running, shows a
//! popover chat pane on click, and mirrors the check-in state in the menubar title.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use focus_core::store::TaskList;
use focus_core::{Event, Paths, Request, Status, client, daemon};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, Rect, WindowEvent, Wry};

const TRAY_ID: &str = "focus";

/// When the popover last hid itself, so the tray click that caused the blur
/// doesn't immediately reopen it.
static LAST_HIDE_MS: AtomicU64 = AtomicU64::new(0);

struct StatusItem(MenuItem<Wry>);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Sends a request and returns only the events meant for this caller. Turn events
/// reach the window through the subscription instead.
async fn send(paths: &Paths, req: Request) -> Result<Vec<Event>, String> {
    let mut direct = Vec::new();
    client::request(paths, req, |ev| {
        if matches!(ev, Event::Ok { .. } | Event::Error { turn: 0, .. }) {
            direct.push(ev);
        }
    })
    .await
    .map_err(|e| format!("{e:#}"))?;
    Ok(direct)
}

#[tauri::command]
async fn say(text: String, paths: tauri::State<'_, Paths>) -> Result<Vec<Event>, String> {
    send(&paths, Request::Say { text }).await
}

/// The popover's buttons and slash commands.
fn control_request(action: &str, arg: Option<String>) -> Result<Request, String> {
    Ok(match action {
        "pause" => Request::Pause,
        "resume" => Request::Resume,
        "next" => Request::Next,
        "report" => Request::Report,
        "add" => Request::Add { url: arg.filter(|a| !a.trim().is_empty()).ok_or("add needs a URL")? },
        "start" => Request::Start { text: arg },
        other => return Err(format!("unknown action {other}")),
    })
}

#[tauri::command]
async fn control(action: String, arg: Option<String>, paths: tauri::State<'_, Paths>) -> Result<Vec<Event>, String> {
    send(&paths, control_request(&action, arg)?).await
}

#[tauri::command]
async fn tasks(paths: tauri::State<'_, Paths>) -> Result<TaskList, String> {
    let mut list = None;
    client::request(&paths, Request::Tasks, |ev| {
        if let Event::Tasks { list: l } = ev {
            list = Some(l);
        }
    })
    .await
    .map_err(|e| format!("{e:#}"))?;
    list.ok_or_else(|| "no task list in reply".into())
}

#[tauri::command]
async fn status(paths: tauri::State<'_, Paths>) -> Result<Status, String> {
    let mut status = None;
    client::request(&paths, Request::Status, |ev| {
        if let Event::Status { status: s } = ev {
            status = Some(s);
        }
    })
    .await
    .map_err(|e| format!("{e:#}"))?;
    status.ok_or_else(|| "no status in reply".into())
}

fn show_popover(app: &AppHandle, anchor: Option<Rect>) {
    let Some(win) = app.get_webview_window("main") else { return };
    if let Some(rect) = anchor
        && let Ok(scale) = win.scale_factor()
    {
        let pos = rect.position.to_physical::<f64>(scale);
        let size = rect.size.to_physical::<f64>(scale);
        let width = win.outer_size().map_or(400.0 * scale, |s| s.width as f64);
        let x = pos.x + size.width / 2.0 - width / 2.0;
        let y = pos.y + size.height + 4.0;
        let _ = win.set_position(PhysicalPosition::new(x.max(0.0), y));
    }
    let _ = win.show();
    let _ = win.set_focus();
}

fn toggle_popover(app: &AppHandle, anchor: Rect) {
    let Some(win) = app.get_webview_window("main") else { return };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
    } else if now_ms().saturating_sub(LAST_HIDE_MS.load(Ordering::Relaxed)) > 300 {
        show_popover(app, Some(anchor));
    }
}

/// Text beside the menubar icon: paused, a check-in waiting on a reply, or nothing.
fn tray_title(status: &Status) -> &'static str {
    if status.paused {
        "⏸"
    } else if status.awaiting_reply {
        "●"
    } else {
        ""
    }
}

fn apply_status(app: &AppHandle, status: &Status) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_title(Some(tray_title(status)));
        let _ = tray.set_tooltip(Some(status.summary()));
    }
    if let Some(item) = app.try_state::<StatusItem>() {
        let _ = item.0.set_text(status.summary());
    }
}

/// Forwards every daemon event to the window and keeps the tray in sync.
async fn subscribe_loop(app: AppHandle, paths: Paths) {
    loop {
        if let Ok(mut sub) = client::subscribe(&paths).await {
            let _ = app.emit("focus-connected", true);
            while let Ok(Some(ev)) = sub.next().await {
                if let Event::Status { status } = &ev {
                    apply_status(&app, status);
                }
                let _ = app.emit("focus-event", &ev);
            }
            let _ = app.emit("focus-connected", false);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn menu_action(app: &AppHandle, id: &str) {
    let paths = app.state::<Paths>().inner().clone();
    let req = match id {
        "open" => return show_popover(app, None),
        "quit" => return app.exit(0),
        "next" => Request::Next,
        "pause" => Request::Pause,
        "resume" => Request::Resume,
        "report" => Request::Report,
        _ => return,
    };
    if matches!(req, Request::Next | Request::Report) {
        show_popover(app, None);
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match send(&paths, req).await {
            Ok(events) => events.iter().for_each(|ev| {
                let _ = app.emit("focus-event", ev);
            }),
            Err(text) => {
                let _ = app.emit("focus-event", Event::Error { turn: 0, text });
            }
        }
    });
}

fn main() {
    let paths = Paths::from_env().expect("resolving FOCUS_HOME");
    // A claude-code turn starts this same binary as its MCP tool server.
    let args: Vec<String> = std::env::args().collect();
    if let [_, cmd, turn] = &args[..]
        && cmd == "mcp"
    {
        let turn = turn.parse().expect("mcp <turn>");
        if let Err(e) = tauri::async_runtime::block_on(focus_core::mcp::serve_stdio(paths, turn)) {
            eprintln!("focus mcp: {e:#}");
        }
        return;
    }

    tauri::Builder::default()
        .manage(paths.clone())
        .invoke_handler(tauri::generate_handler![say, control, tasks, status])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let host = paths.clone();
            tauri::async_runtime::spawn(async move {
                if !client::is_running(&host).await
                    && let Err(e) = daemon::run(host).await
                {
                    eprintln!("focus daemon: {e:#}");
                }
            });
            tauri::async_runtime::spawn(subscribe_loop(app.handle().clone(), paths.clone()));

            let status_item = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &status_item,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "open", "Open Focus Coach", true, None::<&str>)?,
                    &MenuItem::with_id(app, "next", "What's next?", true, None::<&str>)?,
                    &MenuItem::with_id(app, "pause", "Pause check-ins", true, None::<&str>)?,
                    &MenuItem::with_id(app, "resume", "Resume check-ins", true, None::<&str>)?,
                    &MenuItem::with_id(app, "report", "End-of-day report", true, None::<&str>)?,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "quit", "Quit Focus Coach", true, None::<&str>)?,
                ],
            )?;
            app.manage(StatusItem(status_item));

            TrayIconBuilder::with_id(TRAY_ID)
                .icon(Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .icon_as_template(true)
                .tooltip("Focus Coach")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| menu_action(app, event.id().as_ref()))
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        rect,
                        ..
                    } = event
                    {
                        toggle_popover(tray.app_handle(), rect);
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Focused(false) = event {
                LAST_HIDE_MS.store(now_ms(), Ordering::Relaxed);
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("running the menubar app");
}

#[cfg(test)]
#[path = "../../core/tests/support/mod.rs"]
mod support;

#[cfg(test)]
mod tests {
    use super::*;
    use support::{MockLlm, calls, text, write_config};

    fn status(paused: bool, awaiting_reply: bool) -> Status {
        Status {
            paused,
            schedule: "* * * * *".into(),
            next_checkin: None,
            awaiting_reply,
            missed: 0,
            busy: false,
            top: None,
            open: 0,
            session: None,
        }
    }

    #[test]
    fn tray_title_reflects_state() {
        assert_eq!(tray_title(&status(true, true)), "⏸", "paused wins");
        assert_eq!(tray_title(&status(false, true)), "●");
        assert_eq!(tray_title(&status(false, false)), "");
    }

    #[test]
    fn control_actions_map_to_requests() {
        assert!(matches!(control_request("pause", None), Ok(Request::Pause)));
        assert!(matches!(control_request("resume", None), Ok(Request::Resume)));
        assert!(matches!(control_request("next", None), Ok(Request::Next)));
        assert!(matches!(control_request("report", None), Ok(Request::Report)));
        assert!(matches!(control_request("add", Some("https://x.y".into())), Ok(Request::Add { url }) if url == "https://x.y"));
        assert!(control_request("add", None).is_err());
        assert!(control_request("add", Some("  ".into())).is_err());
        assert!(matches!(control_request("start", Some("my PRs".into())), Ok(Request::Start { text: Some(_) })));
        assert!(control_request("explode", None).is_err());
    }

    /// The popover gets direct answers from `send` and turn events only through the
    /// subscription, so nothing shows twice.
    #[tokio::test]
    async fn send_returns_only_direct_answers() {
        let llm = MockLlm::start().await;
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), &llm.url);
        std::fs::write(dir.path().join("tasks.md"), "## Today\n- [ ] Ship {id=t1}\n").unwrap();
        let paths = Paths { home: dir.path().to_path_buf() };
        tokio::spawn(daemon::run(paths.clone()));
        while !client::is_running(&paths).await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut sub = client::subscribe(&paths).await.unwrap();

        let direct = send(&paths, Request::Pause).await.unwrap();
        assert!(matches!(&direct[..], [Event::Ok { text }] if text.contains("paused")), "{direct:?}");

        llm.script([calls(&[("check_off", serde_json::json!({"task_id": "t1"}))]), text("Done.")]);
        let direct = send(&paths, Request::Say { text: "shipped".into() }).await.unwrap();
        assert!(direct.is_empty(), "turn events must not come back directly: {direct:?}");

        let mut saw_reply = false;
        while let Ok(Ok(Some(ev))) = tokio::time::timeout(Duration::from_millis(500), sub.next()).await {
            if matches!(ev, Event::Reply { ref text, .. } if text == "Done.") {
                saw_reply = true;
                break;
            }
        }
        assert!(saw_reply, "the subscription carries the turn's reply");

        let err = send(&Paths { home: dir.path().join("nowhere") }, Request::Status).await.unwrap_err();
        assert!(err.contains("isn't running"), "{err}");
        client::request(&paths, Request::Shutdown, |_| {}).await.unwrap();
    }
}

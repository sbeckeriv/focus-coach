//! Menubar front end. Hosts the daemon in-process when none is running, shows a
//! popover chat pane on click, and mirrors the check-in state in the menubar title.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use focus_core::store::{State, TaskList};
use focus_core::{Event, Paths, Request, Settings, Status, TaskId, client, daemon};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, Rect, WindowEvent, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_notification::{Action, ActionPerformed, ActionType, NotificationExt};

const TRAY_ID: &str = "focus";

/// When the popover last hid itself, so the tray click that caused the blur
/// doesn't immediately reopen it.
static LAST_HIDE_MS: AtomicU64 = AtomicU64::new(0);

/// When a notification button was last pressed, so the app activation it causes
/// doesn't open the popover.
static LAST_BUTTON_MS: AtomicU64 = AtomicU64::new(0);

/// The buttons on a check-in notification, and the actions they run. Done is only
/// offered about a single task; with several active, checking off is for the popover.
const CHECK_IN_BUTTONS: &str = "check-in";
const CHECK_IN_STILL_ONLY: &str = "check-in-still";
const STILL_ON_IT: &str = "still-on-it";
const DONE: &str = "done";

/// What a check-in's buttons are about, as of the daemon's last status: the active
/// tasks, or the top one when none are. Each is its id and its "Title (t3)" label.
struct CheckInAbout(std::sync::Mutex<Vec<(TaskId, String)>>);

impl CheckInAbout {
    fn of(status: &Status) -> Vec<(TaskId, String)> {
        if status.active_ids.is_empty() {
            status.top_id.clone().zip(status.top.clone()).into_iter().collect()
        } else {
            status.active_ids.iter().cloned().zip(status.active.iter().cloned()).collect()
        }
    }
}

struct StatusItem(MenuItem<Wry>);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Sends a request and returns only the events meant for this caller. Turn events
/// reach the window through the subscription instead.
async fn send(paths: &Paths, req: Request) -> Result<Vec<Event>, String> {
    let mut direct = Vec::new();
    client::request(paths, req, |ev| {
        if matches!(ev, Event::Ok { .. } | Event::Error { turn: None, .. }) {
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

/// The popover's buttons, task clicks and slash commands.
fn control_request(action: &str, arg: Option<String>) -> Result<Request, String> {
    let arg = arg.map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
    let need = |what: &str| arg.clone().ok_or_else(|| format!("{action} needs {what}"));
    Ok(match action {
        "pause" => Request::Pause,
        "resume" => Request::Resume,
        "new-session" => Request::NewSession,
        "next" => Request::Next,
        "report" => Request::Report,
        "add" => Request::add(&need("a title or a URL")?, None, None),
        "toggle" => Request::Toggle { id: TaskId::parse(&need("a task id")?) },
        "on" | "off" => Request::SetActive {
            ids: need("task ids")?.split([' ', ',']).filter(|s| !s.is_empty()).map(TaskId::parse).collect(),
            active: action == "on",
        },
        "approve" => Request::Approve { id: need("an action id")?.as_str().into() },
        "reject" => Request::Reject { id: need("an action id")?.as_str().into() },
        "note" => {
            let text = need("a task id and the note")?;
            match text.split_once(char::is_whitespace) {
                Some((id, note)) if !note.trim().is_empty() => Request::Note { id: TaskId::parse(id), text: note.trim().to_string() },
                _ => return Err("note needs a task id and the note, e.g. t3 waiting on legal".into()),
            }
        }
        "start" => Request::Start { text: arg },
        other => return Err(format!("unknown action {other}")),
    })
}

#[tauri::command]
async fn control(action: String, arg: Option<String>, paths: tauri::State<'_, Paths>) -> Result<Vec<Event>, String> {
    send(&paths, control_request(&action, arg)?).await
}

/// Opens a link from the popover in the default browser, not in the popover itself.
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("not a web link: {url}"));
    }
    std::process::Command::new("open").arg(&url).spawn().map_err(|e| format!("couldn't open {url}: {e}"))?;
    Ok(())
}

#[tauri::command]
async fn tasks(paths: tauri::State<'_, Paths>) -> Result<TaskList, String> {
    task_list(&paths).await
}

async fn task_list(paths: &Paths) -> Result<TaskList, String> {
    let mut list = None;
    client::request(paths, Request::Tasks, |ev| {
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

/// Opens the popover whenever macOS activates the app, as a click on one of its
/// notifications does. The app has no Dock icon, so nothing else activates it.
#[cfg(target_os = "macos")]
fn show_popover_on_activate(app: AppHandle) {
    use objc2_app_kit::NSApplicationDidBecomeActiveNotification;
    use objc2_foundation::NSNotificationCenter;

    // A notification button activates the app too; give its handler a moment to say so.
    let block = block2::RcBlock::new(move |_| {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if now_ms().saturating_sub(LAST_BUTTON_MS.load(Ordering::Relaxed)) > 1000 {
                show_popover(&app, None);
            }
        });
    });
    // SAFETY: a system constant; the observer lives as long as the app, so its token is leaked.
    let token = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSApplicationDidBecomeActiveNotification),
            None,
            None,
            &block,
        )
    };
    std::mem::forget(token);
}

fn toggle_popover(app: &AppHandle, anchor: Rect) {
    let Some(win) = app.get_webview_window("main") else { return };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
    } else if now_ms().saturating_sub(LAST_HIDE_MS.load(Ordering::Relaxed)) > 300 {
        show_popover(app, Some(anchor));
    }
}

/// The menubar icon: the template (black, tinted by macOS for light and dark bars), or
/// red while a check-in waits on a reply. The bool remembers which one is showing.
struct TrayIcons {
    plain: Image<'static>,
    waiting: Image<'static>,
    showing_waiting: std::sync::Mutex<bool>,
}

impl TrayIcons {
    fn new() -> tauri::Result<Self> {
        let plain = Image::from_bytes(include_bytes!("../icons/tray.png"))?;
        let mut rgba = plain.rgba().to_vec();
        for px in rgba.as_chunks_mut::<4>().0 {
            px[..3].copy_from_slice(&[0xFF, 0x3B, 0x30]); // systemRed; alpha keeps the shape
        }
        let waiting = Image::new_owned(rgba, plain.width(), plain.height());
        Ok(Self { plain, waiting, showing_waiting: Default::default() })
    }
}

/// Text beside the menubar icon: paused or nothing. A check-in waiting on a reply turns the icon red instead.
fn tray_title(status: &Status) -> &'static str {
    if status.paused {
        "⏸"
    } else {
        ""
    }
}

fn apply_status(app: &AppHandle, status: &Status) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_title(Some(tray_title(status)));
        let _ = tray.set_tooltip(Some(status.summary()));
        if let Some(icons) = app.try_state::<TrayIcons>() {
            let mut showing = icons.showing_waiting.lock().unwrap();
            if *showing != status.awaiting_reply {
                *showing = status.awaiting_reply;
                let icon = if *showing { &icons.waiting } else { &icons.plain };
                let _ = tray.set_icon(Some(icon.clone()));
                let _ = tray.set_icon_as_template(!*showing);
            }
        }
    }
    if let Some(item) = app.try_state::<StatusItem>() {
        let _ = item.0.set_text(status.summary());
    }
    if let Some(about) = app.try_state::<CheckInAbout>() {
        *about.0.lock().unwrap() = CheckInAbout::of(status);
    }
}

/// A native notification for a check-in or note, unless the popover is already in front.
/// Clicking it opens the popover. A check-in also gets "Still on it" about the active
/// tasks (or the top one), and "Done" when that's a single task (see `on_notification_action`).
/// A reminder about an unanswered check-in also plays a sound.
fn notify(app: &AppHandle, text: &str, check_in: bool, reminder: bool) {
    let in_front = app.get_webview_window("main").is_some_and(|w| w.is_visible().unwrap_or(false));
    if in_front {
        return;
    }
    let mut builder = app.notification().builder().title("Focus Coach").body(text);
    if reminder {
        builder = builder.sound("Glass");
    }
    let about = app.try_state::<CheckInAbout>().map(|a| a.0.lock().unwrap().clone()).unwrap_or_default();
    if check_in && !about.is_empty() {
        let buttons = if about.len() == 1 { CHECK_IN_BUTTONS } else { CHECK_IN_STILL_ONLY };
        let ids: Vec<&str> = about.iter().map(|(id, _)| id.as_str()).collect();
        let labels: Vec<&str> = about.iter().map(|(_, label)| label.as_str()).collect();
        builder = builder.action_type_id(buttons).extra("tasks", ids.join(" ")).extra("labels", labels.join(", "));
    }
    let _ = builder.show();
}

/// Whether a check-in opens the popover: once `after` (`checkin_popover`) check-ins in a
/// row are waiting on a reply. 0 never does.
fn opens_popover(after: u32, unanswered: u32) -> bool {
    after > 0 && unanswered >= after
}

/// What a check-in button asks the daemon, about the tasks named in the notification:
/// their ids, space-separated, and their labels.
fn button_request(action: &str, tasks: &str, labels: &str) -> Option<Request> {
    match (action, &tasks.split_whitespace().collect::<Vec<_>>()[..]) {
        (STILL_ON_IT, _) => Some(Request::Say { text: format!("Still on it: {labels}") }),
        (DONE, [one]) => Some(Request::Toggle { id: TaskId::parse(one) }),
        _ => None,
    }
}

fn on_notification_action(app: &AppHandle, performed: &ActionPerformed) {
    let extra = |key| performed.notification().and_then(|n| n.extra().get(key)?.as_str().map(String::from));
    let (Some(tasks), Some(labels)) = (extra("tasks"), extra("labels")) else {
        return show_popover(app, None);
    };
    let Some(req) = button_request(performed.action_id(), &tasks, &labels) else {
        return show_popover(app, None);
    };
    LAST_BUTTON_MS.store(now_ms(), Ordering::Relaxed);
    let (app, paths) = (app.clone(), app.state::<Paths>().inner().clone());
    tauri::async_runtime::spawn(async move {
        // Toggling reopens a done task, so Done only checks off one that's still open.
        if let Request::Toggle { id } = &req
            && let Ok(list) = task_list(&paths).await
            && list.get(id.as_str()).is_ok_and(|t| !matches!(t.state, State::Open))
        {
            return;
        }
        run_request(&app, &paths, req).await;
    });
}

/// Forwards every daemon event to the window, keeps the tray in sync, and shows
/// check-ins as notifications (the daemon skips its own while this is connected).
async fn subscribe_loop(app: AppHandle, paths: Paths) {
    loop {
        if let Ok(mut sub) = client::subscribe_as_notifier(&paths).await {
            let _ = app.emit("focus-connected", true);
            while let Ok(Some(ev)) = sub.next().await {
                match &ev {
                    Event::Status { status } => apply_status(&app, status),
                    Event::CheckIn { text, reminder, unanswered } => {
                        notify(&app, text, true, *reminder);
                        // Read each time, so a focus.toml edit applies without a restart.
                        if Settings::load(&paths).is_ok_and(|s| opens_popover(s.checkin_popover, *unanswered)) {
                            show_popover(&app, None);
                        }
                    }
                    Event::Note { text } => notify(&app, text, false, false),
                    _ => {}
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
        "login" => return toggle_login(app),
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
    tauri::async_runtime::spawn(async move { run_request(&app, &paths, req).await });
}

/// Sends a request from outside the popover and shows its direct answer there.
async fn run_request(app: &AppHandle, paths: &Paths, req: Request) {
    match send(paths, req).await {
        Ok(events) => events.iter().for_each(|ev| {
            let _ = app.emit("focus-event", ev);
        }),
        Err(text) => {
            let _ = app.emit("focus-event", Event::error(text));
        }
    }
}

/// The "Start at login" menu item: a launch agent for this app.
struct LoginItem(CheckMenuItem<Wry>);

fn toggle_login(app: &AppHandle) {
    let autolaunch = app.autolaunch();
    let on = autolaunch.is_enabled().unwrap_or(false);
    let result = if on { autolaunch.disable() } else { autolaunch.enable() };
    if let Err(e) = result {
        let _ = app.emit("focus-event", Event::error(format!("start at login: {e}")));
    }
    if let Some(item) = app.try_state::<LoginItem>() {
        let _ = item.0.set_checked(autolaunch.is_enabled().unwrap_or(false));
    }
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
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::Builder::new().macos_launcher(MacosLauncher::LaunchAgent).build())
        .manage(paths.clone())
        .invoke_handler(tauri::generate_handler![say, control, tasks, status, open_url])
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

            // A click on a check-in notification opens the popover, and its buttons answer
            // it. The plugin reports a click only while the banner is up; one in
            // Notification Center later just activates the app, so activation opens the
            // popover too.
            app.manage(CheckInAbout(Default::default()));
            app.notification().register_action_types(vec![
                ActionType::builder(CHECK_IN_BUTTONS)
                    .actions(vec![Action::builder(STILL_ON_IT, "Still on it").build(), Action::builder(DONE, "Done").build()])
                    .build(),
                ActionType::builder(CHECK_IN_STILL_ONLY).actions(vec![Action::builder(STILL_ON_IT, "Still on it").build()]).build(),
            ])?;
            let handle = app.handle().clone();
            app.notification().on_action(move |performed| on_notification_action(&handle, performed))?;
            #[cfg(target_os = "macos")]
            show_popover_on_activate(app.handle().clone());

            let status_item = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
            let login_on = app.autolaunch().is_enabled().unwrap_or(false);
            let login_item = CheckMenuItem::with_id(app, "login", "Start at login", true, login_on, None::<&str>)?;
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
                    &login_item,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "quit", "Quit Focus Coach", true, None::<&str>)?,
                ],
            )?;
            app.manage(StatusItem(status_item));
            app.manage(LoginItem(login_item));
            let icons = TrayIcons::new()?;
            let plain = icons.plain.clone();
            app.manage(icons);

            TrayIconBuilder::with_id(TRAY_ID)
                .icon(plain)
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
            top_id: None,
            active: vec![],
            active_ids: vec![],
            open: 0,
            session: None,
            pending: vec![],
        }
    }

    #[test]
    fn popover_opens_after_enough_unanswered_check_ins() {
        assert!(!opens_popover(0, 5), "0 never opens it");
        assert!(opens_popover(1, 1), "1 opens it at every check-in");
        assert!(!opens_popover(2, 1));
        assert!(opens_popover(2, 2), "the second check-in in a row without a reply");
        assert!(opens_popover(2, 3));
    }

    #[test]
    fn tray_title_reflects_state() {
        assert_eq!(tray_title(&status(true, true)), "⏸", "paused wins");
        assert_eq!(tray_title(&status(false, true)), "", "a waiting reply turns the icon red instead");
        assert_eq!(tray_title(&status(false, false)), "");
    }

    #[test]
    fn control_actions_map_to_requests() {
        assert!(matches!(control_request("pause", None), Ok(Request::Pause)));
        assert!(matches!(control_request("new-session", None), Ok(Request::NewSession)));
        assert!(matches!(control_request("resume", None), Ok(Request::Resume)));
        assert!(matches!(control_request("next", None), Ok(Request::Next)));
        assert!(matches!(control_request("report", None), Ok(Request::Report)));
        assert!(matches!(control_request("add", Some("https://x.y".into())), Ok(Request::Add { url }) if url == "https://x.y"));
        assert!(matches!(control_request("add", Some("renew passport".into())), Ok(Request::New { title, .. }) if title == "renew passport"));
        assert!(control_request("add", None).is_err());
        assert!(control_request("add", Some("  ".into())).is_err());
        assert!(matches!(control_request("toggle", Some("t3".into())), Ok(Request::Toggle { id }) if id == "t3"));
        assert!(matches!(control_request("approve", Some("a1".into())), Ok(Request::Approve { id }) if id == "a1"));
        assert!(matches!(control_request("reject", Some("a1".into())), Ok(Request::Reject { id }) if id == "a1"));
        assert!(control_request("toggle", None).is_err());
        assert!(matches!(control_request("on", Some("t3 5".into())), Ok(Request::SetActive { ids, active: true }) if ids == ["t3", "t5"]));
        assert!(matches!(control_request("off", Some("t3".into())), Ok(Request::SetActive { active: false, .. })));
        assert!(control_request("on", None).is_err());
        assert!(matches!(control_request("note", Some("3 waiting on legal".into())), Ok(Request::Note { id, text }) if id == "t3" && text == "waiting on legal"));
        assert!(control_request("note", Some("t3".into())).is_err(), "a note needs text");
        assert!(matches!(control_request("start", Some("my PRs".into())), Ok(Request::Start { text: Some(_) })));
        assert!(control_request("explode", None).is_err());
    }

    #[test]
    fn check_in_buttons_answer_about_the_top_task() {
        assert!(matches!(button_request(STILL_ON_IT, "t3", "Review configs (t3)"),
            Some(Request::Say { text }) if text == "Still on it: Review configs (t3)"));
        assert!(matches!(button_request(DONE, "t3", "Review configs (t3)"), Some(Request::Toggle { id }) if id == "t3"));
        assert!(button_request("tap", "t3", "Review configs (t3)").is_none(), "a click on the body opens the popover");
        assert!(matches!(button_request(STILL_ON_IT, "t3 t5", "Deploy (t3), Docs (t5)"),
            Some(Request::Say { text }) if text == "Still on it: Deploy (t3), Docs (t5)"));
        assert!(button_request(DONE, "t3 t5", "Deploy (t3), Docs (t5)").is_none(), "Done is only about one task");

        let mut s = status(false, false);
        (s.top_id, s.top) = (Some("t1".into()), Some("Ship (t1)".into()));
        assert_eq!(CheckInAbout::of(&s), vec![("t1".into(), "Ship (t1)".into())], "no active tasks: the top one");
        (s.active_ids, s.active) = (vec!["t3".into(), "t5".into()], vec!["Deploy (t3)".into(), "Docs (t5)".into()]);
        assert_eq!(CheckInAbout::of(&s).len(), 2, "the active tasks, not the top one");
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

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const feed = $("feed");
const input = $("input");
let paused = false;
// The daemon decides the top item (Status.top_id); the popover only marks it.
let topId = null;
let pending = 0;

function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

function push(cls, text) {
  const node = el("div", `msg ${cls}`, text);
  feed.appendChild(node);
  feed.scrollTop = feed.scrollHeight;
  return node;
}

function setThinking() {
  let node = feed.querySelector(".thinking");
  if (pending > 0 && !node) {
    node = push("thinking", "thinking…");
  }
  if (node) {
    feed.appendChild(node); // keep it last
    node.hidden = pending === 0;
  }
}

function renderStatus(s) {
  paused = s.paused;
  topId = s.top_id;
  const status = $("status");
  status.classList.toggle("paused", s.paused);
  status.textContent = summarize(s);
  $("toggle").textContent = s.paused ? "Resume" : "Pause";
}

function summarize(s) {
  const timer = s.paused
    ? "paused until your next message"
    : s.next_checkin
      ? `next check-in ${new Date(s.next_checkin).toLocaleTimeString([], { weekday: "short", hour: "2-digit", minute: "2-digit" })}`
      : "no upcoming check-in";
  const approvals = s.pending?.length ? ` · ${s.pending.length} awaiting approval` : "";
  return `${s.open} open · ${timer}${s.awaiting_reply ? " · waiting on your reply" : ""}${approvals}`;
}

const MARKS = { open: "○", done: "✓", on_hold: "‖", blocked: "!" };

// The task whose notes and links are open, if any.
let openTask = null;

/// Notes, links and a box to add a note, under a clicked task.
function details(task) {
  const box = el("div", "details");
  for (const note of task.notes ?? []) box.appendChild(el("p", "", `note: ${note}`));
  for (const link of task.links ?? []) box.appendChild(el("p", "muted", link));
  if (task.blocked_by) box.appendChild(el("p", "muted", `blocked by ${task.blocked_by}`));
  const add = el("input");
  add.placeholder = "Add a note…";
  add.addEventListener("keydown", (e) => {
    if (e.key !== "Enter" || !add.value.trim()) return;
    run(invoke("control", { action: "note", arg: `${task.id} ${add.value.trim()}` }));
    add.value = "";
  });
  box.appendChild(add);
  return box;
}

function renderTasks(list) {
  const root = $("tasks");
  root.replaceChildren();
  const today = new Date().toLocaleDateString("en-CA");
  const top = topId;
  for (const section of list.sections) {
    const visible = section.tasks.filter((t) => t.state !== "done" || t.done_on === today);
    if (!visible.length && !section.on_hold) continue;
    const box = el("div", `section${section.on_hold ? " held" : ""}`);
    box.appendChild(el("h3", "", section.on_hold ? `${section.name} · on hold` : section.name));
    if (!section.on_hold) {
      for (const task of visible) {
        const row = el("div", `task ${task.state}${task.id === top ? " top" : ""}`);
        const mark = el("button", "mark", task.id === top ? "▶" : MARKS[task.state]);
        mark.type = "button";
        mark.title = task.state === "done" ? "Reopen" : "Check off";
        mark.addEventListener("click", () => run(invoke("control", { action: "toggle", arg: task.id })));
        row.appendChild(mark);
        const name = el("span", "name", task.title);
        name.title = "Show notes and links";
        name.addEventListener("click", () => {
          openTask = openTask === task.id ? null : task.id;
          renderTasks(list);
        });
        row.appendChild(name);
        if (task.notes?.length) row.appendChild(el("span", "notes", "✎"));
        if (task.due) row.appendChild(el("span", "due", `due ${task.due.slice(5)}`));
        row.appendChild(el("span", "id", task.id));
        box.appendChild(row);
        if (openTask === task.id) box.appendChild(details(task));
      }
    }
    root.appendChild(box);
  }
  if (!root.children.length) {
    root.appendChild(el("div", "empty", "No tasks yet. Try: look at my PRs and make a task list."));
  }
}

async function refreshTasks() {
  try {
    renderTasks(await invoke("tasks"));
  } catch {
    // Daemon not up yet; the next event or tick retries.
  }
}

function onEvent(ev) {
  switch (ev.type) {
    case "check_in": push("checkin", ev.text); break;
    case "note": push("coach", `Note to self: ${ev.text}`); break;
    case "proposal": {
      const p = ev.proposal;
      const node = push("preview proposal", "");
      node.appendChild(el("span", "label", `${p.id} · ${p.kind} to ${p.target} · awaiting approval`));
      node.appendChild(document.createTextNode(p.content));
      const buttons = el("div", "choices");
      for (const [action, label] of [["approve", "Approve"], ["reject", "Reject"]]) {
        const b = el("button", action, label);
        b.type = "button";
        b.addEventListener("click", () => {
          buttons.remove();
          run(invoke("control", { action, arg: p.id }));
        });
        buttons.appendChild(b);
      }
      node.appendChild(buttons);
      break;
    }
    case "reply": case "ok": push("coach", ev.text); break;
    case "question": push("question", ev.text); break;
    case "preview": {
      const node = push("preview", "");
      node.appendChild(el("span", "label", `${ev.kind} · preview, not sent`));
      node.appendChild(document.createTextNode(ev.content));
      break;
    }
    case "changed": push("change", `· ${ev.summary}`); refreshTasks(); break;
    case "report": push("coach", `${ev.text}\n\nSaved to ${ev.path}`); break;
    case "error": push("error", ev.text); break;
    case "status": renderStatus(ev.status); refreshTasks(); break;
  }
  setThinking();
}

async function run(promise) {
  pending++;
  setThinking();
  try {
    (await promise).forEach(onEvent);
  } catch (e) {
    push("error", String(e));
  } finally {
    pending--;
    setThinking();
    refreshTasks();
  }
}

function slash(text) {
  const [cmd, ...rest] = text.split(" ");
  const arg = rest.join(" ").trim() || null;
  const actions = {
    "/next": "next", "/pause": "pause", "/resume": "resume", "/report": "report",
    "/add": "add", "/start": "start", "/approve": "approve", "/reject": "reject", "/note": "note",
  };
  return actions[cmd] ? { action: actions[cmd], arg } : null;
}

$("composer").addEventListener("submit", (e) => {
  e.preventDefault();
  const text = input.value.trim();
  if (!text) return;
  input.value = "";
  input.style.height = "";
  push("you", text);
  const cmd = slash(text);
  run(cmd ? invoke("control", cmd) : invoke("say", { text }));
});

input.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    $("composer").requestSubmit();
  }
});

input.addEventListener("input", () => {
  input.style.height = "";
  input.style.height = `${input.scrollHeight}px`;
});

$("next").addEventListener("click", () => run(invoke("control", { action: "next" })));
$("report").addEventListener("click", () => run(invoke("control", { action: "report" })));
$("toggle").addEventListener("click", () => run(invoke("control", { action: paused ? "resume" : "pause" })));

// Drag the bar between the task list and the conversation to resize them. The height
// is remembered in this window only; storage can be unavailable, which is fine.
const splitter = $("splitter");
try {
  const saved = localStorage.getItem("tasks-height");
  if (saved) document.body.style.setProperty("--tasks-height", saved);
} catch {}
splitter.addEventListener("pointerdown", (e) => {
  splitter.setPointerCapture(e.pointerId);
  splitter.classList.add("dragging");
});
splitter.addEventListener("pointermove", (e) => {
  if (!splitter.hasPointerCapture(e.pointerId)) return;
  const top = $("tasks").getBoundingClientRect().top;
  const max = window.innerHeight - 160; // leave room for the conversation and input
  const height = Math.max(48, Math.min(max, e.clientY - top));
  document.body.style.setProperty("--tasks-height", `${height}px`);
});
splitter.addEventListener("pointerup", (e) => {
  splitter.releasePointerCapture(e.pointerId);
  splitter.classList.remove("dragging");
  try {
    localStorage.setItem("tasks-height", document.body.style.getPropertyValue("--tasks-height"));
  } catch {}
});

listen("focus-event", (e) => onEvent(e.payload));
listen("focus-connected", (e) => {
  if (!e.payload) $("status").textContent = "reconnecting to the daemon…";
});
window.addEventListener("focus", () => {
  input.focus();
  refreshTasks();
});

invoke("status").then(renderStatus).catch(() => {});
refreshTasks();
setInterval(refreshTasks, 10000);

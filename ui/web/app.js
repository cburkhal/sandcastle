// The window's logic. It only calls the app's own commands; all text from
// the system (repo paths, branch names, errors) is set as text, never as HTML.
const { invoke } = window.__TAURI__.core;
const pickFolder = window.__TAURI__.dialog.open;
const $ = (id) => document.getElementById(id);

let current = { unlocked: false, firewall: true, needs_password: true, boxes: [] };
let selected = null;
let busy = false;

function showError(error) {
  $("error").textContent = String(error);
  $("error").hidden = false;
}

// Run one command; the window is inert while it runs.
async function run(message, action) {
  if (busy) return;
  busy = true;
  $("error").hidden = true;
  $("busy").textContent = message;
  $("busy").hidden = false;
  render();
  try {
    await action();
  } catch (error) {
    showError(error);
  }
  busy = false;
  $("busy").hidden = true;
  await refresh();
}

async function refresh() {
  if (busy) return;
  try {
    current = await invoke("status");
  } catch (error) {
    showError(error);
  }
  render();
}

function render() {
  const box = current.boxes.find((b) => b.name === selected) || current.boxes[0] || null;
  selected = box ? box.name : null;

  const list = $("boxes");
  list.replaceChildren(...current.boxes.map((b) => {
    const button = document.createElement("button");
    button.className = b.running ? "running" : "";
    button.setAttribute("aria-current", String(b.name === selected));
    const dot = document.createElement("span");
    dot.className = "dot";
    dot.setAttribute("aria-hidden", "true");
    const label = document.createElement("span");
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = b.repo.split("/").pop();
    const state = document.createElement("span");
    state.className = "state";
    state.textContent = b.running ? "Running" : "Stopped";
    label.append(name, state);
    button.append(dot, label);
    button.addEventListener("click", () => { selected = b.name; $("branches").replaceChildren(); render(); });
    return button;
  }));

  $("session-state").textContent = current.unlocked ? "Secrets in memory, vault locked" : "Locked";
  $("firewall-state").textContent = current.firewall
    ? (current.unlocked ? "Firewall helper active" : "")
    : "WARNING: started with --no-firewall. Boxes can reach other services on this Mac.";
  $("lock").hidden = !current.unlocked;
  $("lock").disabled = busy;
  $("add").disabled = busy || !current.unlocked;

  $("unlock").hidden = current.unlocked || busy;
  $("password-row").hidden = !current.needs_password;
  $("sudo-hint").hidden = !current.firewall;

  const showDetail = current.unlocked && box !== null;
  $("detail").hidden = !showDetail;
  $("empty").hidden = !(current.unlocked && box === null) || busy;
  if (showDetail) {
    $("box-name").textContent = box.repo.split("/").pop();
    $("box-repo").textContent = box.repo;
    $("toggle").textContent = box.running ? "Stop" : "Start";
    $("chat").disabled = busy || !box.running;
    $("fetch").disabled = busy || !box.running;
    $("toggle").disabled = busy;
  }
}

$("unlock").addEventListener("submit", (event) => {
  event.preventDefault();
  const field = $("password");
  const password = field.value;
  field.value = "";
  run("Unlocking. Confirm the macOS prompt if one appears.", () => invoke("unlock", { password }));
});

$("lock").addEventListener("click", () => run("Stopping all boxes.", () => invoke("lock")));

$("add").addEventListener("click", async () => {
  const path = await pickFolder({ directory: true, title: "Choose a folder" });
  if (path) run("Starting the box. The first start of a Git repository clones it.", () => invoke("start_box", { path }));
});

$("toggle").addEventListener("click", () => {
  const box = current.boxes.find((b) => b.name === selected);
  if (!box) return;
  if (box.running) run("Stopping the box.", () => invoke("stop_box", { name: box.name }));
  else run("Starting the box.", () => invoke("start_box", { path: box.repo }));
});

$("chat").addEventListener("click", () => run("Opening the chat in your browser.", () => invoke("open_chat", { name: selected })));

$("fetch").addEventListener("click", () => run("Fetching the agent's commits.", async () => {
  const branches = await invoke("fetch", { name: selected });
  const lines = branches.length ? branches : ["The box has no branches yet."];
  $("branches").replaceChildren(...lines.map((line) => {
    const item = document.createElement("li");
    item.textContent = line;
    return item;
  }));
}));

refresh();
setInterval(refresh, 5000);

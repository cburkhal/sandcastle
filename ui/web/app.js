// The window's logic. It only calls the app's own commands; all text from
// the system (repo paths, branch names, errors) is set as text, never as HTML.
const { invoke } = window.__TAURI__.core;
const pickFolder = window.__TAURI__.dialog.open;
const $ = (id) => document.getElementById(id);

let current = { unlocked: false, firewall: true, needs_password: true, boxes: [], problems: [], notices: [] };
let selected = null;
// The command that runs: { kind, box } or null. The window is inert meanwhile.
let busy = null;
// The box whose deletion waits for a confirmation, or null.
let confirming = null;
// What the last review of a push request showed: { box, branch, text } or null.
let reviewed = null;
// The last error of a background refresh, so it is not shown again and again.
let refreshError = null;

const folderName = (path) => path.split("/").filter(Boolean).pop() || path;

// An error stays until it is dismissed or the next command starts.
function showError(title, error) {
  $("error-title").textContent = title;
  $("error-detail").textContent = error instanceof Error ? error.message : String(error);
  $("error").hidden = false;
}

function addNotices(notices) {
  for (const notice of notices) {
    const item = document.createElement("li");
    const text = document.createElement("span");
    text.textContent = notice;
    const close = document.createElement("button");
    close.textContent = "Dismiss";
    close.addEventListener("click", () => item.remove());
    item.append(text, close);
    $("notices").append(item);
  }
}

// Run one command. `what` says what is shown while it runs and if it fails:
// { busy, failed, kind, box }. Returns whether it succeeded.
async function run(what, action) {
  if (busy) return false;
  busy = { kind: what.kind, box: what.box || null };
  $("error").hidden = true;
  $("busy-text").textContent = what.busy;
  $("busy").hidden = false;
  render();
  // The app reports what a long command is waiting for.
  const poll = setInterval(async () => {
    try {
      const step = await invoke("progress");
      if (busy && step) $("busy-text").textContent = step;
    } catch (_) { /* the fixed text stays */ }
  }, 400);
  let ok = true;
  try {
    await action();
  } catch (error) {
    ok = false;
    showError(what.failed, error);
  }
  clearInterval(poll);
  busy = null;
  $("busy").hidden = true;
  await refresh();
  return ok;
}

async function refresh() {
  if (busy) return;
  try {
    current = await invoke("status");
    refreshError = null;
    addNotices(current.notices);
  } catch (error) {
    const message = String(error);
    if (message !== refreshError) showError("Could not read sandcastle's state", error);
    refreshError = message;
  }
  render();
}

function boxRow(box) {
  const row = document.createElement("div");
  row.className = "box" + (box.running ? " running" : "");
  row.setAttribute("aria-current", String(box.name === selected));

  const select = document.createElement("button");
  select.className = "select";
  const dot = document.createElement("span");
  dot.className = "dot";
  dot.setAttribute("aria-hidden", "true");
  const label = document.createElement("span");
  const name = document.createElement("span");
  name.className = "name";
  name.textContent = folderName(box.repo);
  const state = document.createElement("span");
  state.className = "state";
  const mine = busy && busy.box === box.name;
  state.textContent = mine && busy.kind === "start" ? "Starting…"
    : mine && busy.kind === "stop" ? "Stopping…"
    : box.running ? "Running" : "Stopped";
  if (box.requests.length > 0) state.textContent += ` · ${box.requests.length} to publish`;
  label.append(name, state);
  select.append(dot, label);
  select.addEventListener("click", () => {
    selected = box.name;
    confirming = null;
    $("branches").replaceChildren();
    render();
  });

  const toggle = document.createElement("button");
  toggle.className = "toggle";
  toggle.textContent = box.running ? "Stop" : "Start";
  toggle.setAttribute("aria-label", `${box.running ? "Stop" : "Start"} the box for ${folderName(box.repo)}`);
  toggle.disabled = Boolean(busy) || !current.unlocked;
  if (!current.unlocked) toggle.title = "Unlock sandcastle first";
  toggle.addEventListener("click", () => {
    selected = box.name;
    if (box.running) {
      run({ busy: "Stopping the box.", failed: `Could not stop the box for ${folderName(box.repo)}`, kind: "stop", box: box.name },
        () => invoke("stop_box", { name: box.name }));
    } else {
      startBox(box.repo, box.name);
    }
  });

  row.append(select, toggle);
  return row;
}

function render() {
  const box = current.boxes.find((b) => b.name === selected) || current.boxes[0] || null;
  selected = box ? box.name : null;
  const working = Boolean(busy);
  const unlocking = working && busy.kind === "unlock";

  $("boxes").replaceChildren(...current.boxes.map(boxRow));

  $("session-state").textContent = current.unlocked ? "Secrets in memory, vault locked" : "Locked";
  $("firewall-state").textContent = current.firewall
    ? (current.unlocked ? "Firewall helper active" : "")
    : "WARNING: started with --no-firewall. Boxes can reach other services on this Mac.";
  $("lock").hidden = !current.unlocked;
  $("lock").disabled = working;
  $("add").disabled = working || !current.unlocked;

  // Locked: either what must be fixed first, or the unlock form. The form
  // stays visible while unlocking, so it is clear that something happens.
  const blocked = !current.unlocked && current.problems.length > 0;
  $("setup").hidden = !blocked;
  $("problems").replaceChildren(...current.problems.map((problem) => {
    const item = document.createElement("li");
    item.textContent = problem;
    return item;
  }));
  $("unlock").hidden = current.unlocked || blocked;
  $("password-row").hidden = !current.needs_password;
  $("sudo-hint").hidden = !current.firewall;
  $("password").disabled = working;
  $("unlock-button").disabled = working;
  $("unlock-button").textContent = unlocking ? "Unlocking…" : "Unlock";

  const showDetail = current.unlocked && box !== null;
  $("detail").hidden = !showDetail;
  $("empty").hidden = !(current.unlocked && box === null) || working;
  if (showDetail) {
    $("box-name").textContent = folderName(box.repo);
    $("box-repo").textContent = box.repo;
    const mine = working && busy.box === box.name;
    $("box-state").textContent = mine && busy.kind === "start" ? "Starting…"
      : mine && busy.kind === "stop" ? "Stopping…"
      : box.running ? "Running" : "Stopped. Start it in the list on the left to chat or fetch.";
    renderPublishing(box, working);
    if (confirming !== box.name) confirming = null;
    $("delete").hidden = confirming !== null;
    $("delete").disabled = working;
    $("confirm-delete").hidden = confirming === null;
    $("confirm-title").textContent = `Delete the box for ${box.repo}?`
      + (box.running ? " It is running and will be stopped first." : "");
    $("delete-yes").disabled = working;
    $("delete-no").disabled = working;
    $("chat").disabled = working || !box.running;
    $("fetch").disabled = working || !box.running;
  }
}

function renderPublishing(box, working) {
  for (const radio of document.querySelectorAll('input[name="push-mode"]')) {
    radio.checked = radio.value === box.push_mode;
    radio.disabled = working;
  }
  $("push-warning").hidden = box.push_mode !== "always";

  $("requests").replaceChildren(...box.requests.map((request) => {
    const item = document.createElement("li");
    const label = document.createElement("span");
    label.className = "mono";
    label.textContent = request.branch + (request.reason ? `  (${request.reason})` : "");
    const actions = document.createElement("span");
    actions.className = "actions";
    const button = (text, className, onClick) => {
      const b = document.createElement("button");
      b.textContent = text;
      b.className = className;
      // Reading the box's branches needs the running box.
      b.disabled = working || (!box.running && text !== "Drop");
      b.addEventListener("click", onClick);
      return b;
    };
    actions.append(
      button("Review", "", () => run(
        { busy: `Fetching ${request.branch} from the box.`, failed: `Could not review ${request.branch}`, kind: "review" },
        async () => {
          const summary = await invoke("review_push", { name: box.name, branch: request.branch });
          const lines = summary.commits.length
            ? [`${summary.commits.length} new commit(s) on ${summary.branch}:`, ...summary.commits.map((c) => `  ${c}`), "", summary.stat]
            : [`Nothing new on ${summary.branch}: every commit is already in your repository or on its remote.`];
          if (summary.touches_ci) lines.push("", "WARNING: these commits change CI configuration. CI may run them with its secrets once published.");
          lines.push("", `Read the changes in a terminal: git log -p sandcastle/${summary.branch} --not --branches --remotes`);
          reviewed = { box: box.name, branch: request.branch, text: lines.join("\n") };
        },
      )),
      button("Publish", "primary", () => run(
        { busy: `Publishing ${request.branch} from this Mac.`, failed: `Could not publish ${request.branch}`, kind: "publish" },
        async () => {
          addNotices([await invoke("approve_push", { name: box.name, branch: request.branch })]);
          reviewed = null;
        },
      )),
      button("Drop", "", () => run(
        { busy: "Dropping the request.", failed: `Could not drop the request for ${request.branch}`, kind: "deny" },
        async () => {
          await invoke("deny_push", { name: box.name, branch: request.branch });
          reviewed = null;
        },
      )),
    );
    item.append(label, actions);
    return item;
  }));

  const showReview = reviewed !== null && reviewed.box === box.name
    && box.requests.some((request) => request.branch === reviewed.branch);
  $("review").hidden = !showReview;
  $("review").textContent = showReview ? reviewed.text : "";
}

for (const radio of document.querySelectorAll('input[name="push-mode"]')) {
  radio.addEventListener("change", () => {
    const box = current.boxes.find((b) => b.name === selected);
    if (!box || !radio.checked) return;
    run({ busy: "Saving.", failed: "Could not change the setting", kind: "mode" },
      () => invoke("set_push_mode", { name: box.name, mode: radio.value }));
  });
}

function startBox(path, name) {
  return run({
    busy: "Starting the box. The first start of a Git repository clones it; this can take a minute.",
    failed: `Could not start the box for ${folderName(path)}`,
    kind: "start",
    box: name,
  }, () => invoke("start_box", { path }));
}

$("error-close").addEventListener("click", () => { $("error").hidden = true; });

$("unlock").addEventListener("submit", async (event) => {
  event.preventDefault();
  const field = $("password");
  const password = field.value;
  const missing = current.needs_password && password === "";
  $("password-problem").hidden = !missing;
  if (missing) {
    field.focus();
    return;
  }
  field.value = "";
  const ok = await run({ busy: "Unlocking.", failed: "Could not unlock", kind: "unlock" }, () => invoke("unlock", { password }));
  if (!ok) field.focus();
});

$("lock").addEventListener("click", () => run({ busy: "Stopping all boxes and locking.", failed: "Locking had a problem", kind: "lock" }, () => invoke("lock")));

$("add").addEventListener("click", async () => {
  let path = null;
  try {
    path = await pickFolder({ directory: true, title: "Choose a folder" });
  } catch (error) {
    showError("Could not open the folder picker", error);
  }
  if (path) startBox(path, null);
});

$("delete").addEventListener("click", () => { confirming = selected; render(); $("delete-no").focus(); });
$("delete-no").addEventListener("click", () => { confirming = null; render(); });
$("delete-yes").addEventListener("click", async () => {
  const box = current.boxes.find((b) => b.name === confirming);
  if (!box) return;
  const ok = await run(
    { busy: "Deleting the box.", failed: `Could not delete the box for ${folderName(box.repo)}`, kind: "delete", box: box.name },
    () => invoke("delete_box", { name: box.name }),
  );
  if (ok) { confirming = null; $("branches").replaceChildren(); render(); }
});

$("chat").addEventListener("click", () => run(
  { busy: "Opening the chat in your browser.", failed: "Could not open the chat", kind: "chat" },
  () => invoke("open_chat", { name: selected }),
));

$("fetch").addEventListener("click", () => run(
  { busy: "Fetching the agent's commits.", failed: "Could not fetch the agent's commits", kind: "fetch" },
  async () => {
    const branches = await invoke("fetch", { name: selected });
    const lines = branches.length ? branches : ["The box has no branches yet."];
    $("branches").replaceChildren(...lines.map((line) => {
      const item = document.createElement("li");
      item.textContent = line;
      return item;
    }));
  },
));

refresh();
setInterval(refresh, 5000);

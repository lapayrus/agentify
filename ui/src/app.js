// Wires the shell to the daemon. This file owns the DOM and the polling; all
// rendering lives in render.js. No business logic belongs here: the shell asks
// the daemon (through the Rust commands) and paints what it is told.
//
// Live logs are a cursor, not a second source of truth: the shell asks for
// lines newer than `nextSince` and appends them. The same holds for the
// Approval inbox and the activity feed: both are reads over what the daemon
// recorded, and both are re-read after every decision.

import { render, projectsOf } from "./render.js";

const POLL_INTERVAL_MS = 5000;
const LOGS_INTERVAL_MS = 1500;
const LOG_BUFFER = 500;
const DEFAULT_LOG_TAIL = 100;
/** How many decided Approvals the card column keeps in view. */
const APPROVAL_LIMIT = 12;
/** How many audit entries the feed shows. */
const ACTIVITY_LIMIT = 40;
/** How often a background update check runs while the window is open. */
const UPDATE_INTERVAL_MS = 6 * 60 * 60 * 1000;
/** Where the shell keeps its one local preference. */
const PREFERENCES_KEY = "agentify.preferences";
const app = document.getElementById("app");

let status = { state: "loading" };
let selected = null;
let tab = "overview";
/** `projects`, `mcp`, or `settings`: the top-level section in the header. */
let section = "projects";
/** `{ autoUpdate }`, persisted in the webview. */
let preferences = loadPreferences();
/** What `shell_info` reported: the app version, data root, and MCP connector. */
let shellInfo = null;
let shellInfoError = null;
/** The last update check: status, versions, notes, and whether it was dismissed. */
let update = {
  status: "idle",
  currentVersion: null,
  version: null,
  body: null,
  error: null,
  dismissed: false,
};
/** The first-run state: `{ state, message, elevated_step, reboot_required }`. */
let bootstrap = null;
/** { status, stats, logs: { lines, nextSince, following, error }, pending, error } */
let view = { status: null, stats: null, logs: { lines: [], following: false }, pending: null };
/** The right column: the Approval inbox and the activity feed. */
let inbox = {
  approvals: [],
  activity: null,
  decision: null,
  approvalsError: null,
  activityError: null,
};
let logsTimer = null;

function bridge() {
  const tauri = window.__TAURI__;
  return tauri?.core?.invoke ? tauri.core : null;
}

async function call(command, args) {
  const core = bridge();
  if (!core) {
    throw new Error(
      "This page is not running inside the Agentify desktop shell, so there is no daemon bridge to call.",
    );
  }
  return core.invoke(command, args);
}

async function askDaemon() {
  if (!bridge()) {
    return {
      state: "unreachable",
      reason:
        "This page is not running inside the Agentify desktop shell, so there is no daemon bridge to call.",
    };
  }
  try {
    return await call("daemon_status");
  } catch (error) {
    return {
      state: "unreachable",
      reason: `The shell could not ask the daemon: ${error}`,
    };
  }
}

/** Keep the selection if the daemon still lists it, otherwise pick the first. */
function reconcileSelection() {
  const projects = projectsOf(status);
  if (projects.length === 0) {
    selected = null;
    return;
  }
  const keep = projects.find(
    (project) => project.id === selected || project.name === selected,
  );
  selected = (keep ?? projects[0]).id;
}

function selectedProjectName() {
  const project = projectsOf(status).find(
    (candidate) => candidate.id === selected || candidate.name === selected,
  );
  return project?.name ?? null;
}

/** Environment status, stats, and the first page of logs for the selection. */
async function loadView({ resetLogs = false } = {}) {
  const project = selectedProjectName();
  if (!project) {
    view = { status: null, stats: null, logs: { lines: [], following: false }, pending: null };
    return;
  }
  try {
    const [environment, stats] = await Promise.all([
      call("environment_status", { project }),
      call("environment_stats", { project }),
    ]);
    view = { ...view, status: environment, stats, error: null };
  } catch (error) {
    view = { ...view, error: String(error) };
  }
  if (resetLogs || view.logs.lines.length === 0) {
    await followLogs({ reset: true });
  }
}

/** Fetch the lines after the cursor and append them. */
async function followLogs({ reset = false } = {}) {
  const project = selectedProjectName();
  if (!project) {
    return;
  }
  const since = reset ? null : view.logs.nextSince;
  try {
    const page = await call("environment_logs", {
      project,
      tail: DEFAULT_LOG_TAIL,
      since,
    });
    const incoming = Array.isArray(page?.lines) ? page.lines : [];
    const lines = reset ? incoming : [...view.logs.lines, ...incoming];
    view = {
      ...view,
      logs: {
        lines: lines.slice(-LOG_BUFFER),
        nextSince: page?.nextSince ?? since,
        following: tab === "logs",
        error: null,
      },
    };
  } catch (error) {
    view = { ...view, logs: { ...view.logs, error: String(error) } };
  }
}

/**
 * The Approval inbox and the activity feed. Both are reads, so a failure here
 * is reported beside the column rather than replacing the Project pane.
 */
async function loadInbox() {
  if (!bridge()) {
    return;
  }
  try {
    const approvals = await call("approvals", { state: "all", limit: APPROVAL_LIMIT });
    inbox = {
      ...inbox,
      approvals: Array.isArray(approvals?.approvals) ? approvals.approvals : [],
      approvalsError: null,
    };
  } catch (error) {
    inbox = { ...inbox, approvals: [], approvalsError: String(error) };
  }
  try {
    inbox = { ...inbox, activity: await call("activity", { limit: ACTIVITY_LIMIT }), activityError: null };
  } catch (error) {
    inbox = { ...inbox, activity: null, activityError: String(error) };
  }
}

/**
 * Approve or deny. The daemon is the gate — approving asks the OS for the
 * gesture — so the shell only reports what came back and re-reads both lists.
 */
async function decide(action, approvalId) {
  inbox = { ...inbox, decision: { id: approvalId, action }, approvalsError: null };
  paint();
  try {
    const result = await call("decide_approval", {
      approval: approvalId,
      approve: action === "approve",
    });
    if (result && result.ok === false) {
      inbox = {
        ...inbox,
        approvalsError: result.detail ?? `${action} failed (${result.code ?? "unknown"})`,
      };
    }
  } catch (error) {
    inbox = { ...inbox, approvalsError: String(error) };
  }
  inbox = { ...inbox, decision: null };
  await loadInbox();
  await loadView();
  paint();
}

function loadPreferences() {
  try {
    const parsed = JSON.parse(window.localStorage.getItem(PREFERENCES_KEY) ?? "{}");
    return { autoUpdate: parsed?.autoUpdate !== false };
  } catch {
    return { autoUpdate: true };
  }
}

function savePreferences() {
  try {
    window.localStorage.setItem(PREFERENCES_KEY, JSON.stringify(preferences));
  } catch {
    // Storage may be disabled; the preference then lasts for this window only.
  }
}

/** The app version, data root, and MCP connector details. A read. */
async function loadShellInfo() {
  if (!bridge()) {
    return;
  }
  try {
    shellInfo = await call("shell_info");
    shellInfoError = null;
  } catch (error) {
    shellInfoError = String(error);
  }
  paint();
}

/**
 * Ask the updater whether a signed release is newer than this build. A manual
 * check surfaces a failure; a background one stays quiet, because being offline
 * is not something the window needs to shout about.
 */
async function checkForUpdate({ manual = false } = {}) {
  if (!bridge()) {
    return;
  }
  if (manual) {
    update = { ...update, status: "checking", error: null, dismissed: false };
    paint();
  }
  try {
    const result = await call("update_check");
    const available = result?.update ?? null;
    const version = available?.version ?? null;
    update = {
      status: available ? "available" : "current",
      currentVersion: result?.current_version ?? update.currentVersion,
      version,
      body: available?.body ?? null,
      error: null,
      dismissed: update.dismissed && update.version === version,
    };
  } catch (error) {
    update = { ...update, status: manual ? "error" : "idle", error: String(error) };
  }
  paint();
}

/**
 * Download and install the update. The updater restarts the shell; the daemon is
 * stopped first so the installer can replace its sidecar.
 */
async function installUpdate() {
  update = { ...update, status: "installing", error: null, dismissed: false };
  paint();
  try {
    await call("update_install");
    // The updater restarts the shell on Windows. If it returns instead, bring
    // the daemon back up on the new build.
    await call("restart_daemon").catch(() => {});
  } catch (error) {
    update = { ...update, status: "error", error: String(error) };
    paint();
  }
}

function paint() {
  app.dataset.connection = status.state;
  app.innerHTML = render({
    ...status,
    selected,
    view,
    inbox,
    bootstrap,
    section,
    update,
    shellInfo,
    shellInfoError,
    preferences,
  });

  for (const button of app.querySelectorAll("[data-nav]")) {
    button.addEventListener("click", () => {
      const next = button.dataset.nav;
      if (!["projects", "mcp", "settings"].includes(next) || next === section) {
        return;
      }
      section = next;
      paint();
      if (next === "mcp" && shellInfo === null && shellInfoError === null) {
        loadShellInfo();
      }
    });
  }

  for (const input of app.querySelectorAll("[data-setting]")) {
    input.addEventListener("change", () => {
      if (input.dataset.setting !== "auto-update") {
        return;
      }
      preferences = { ...preferences, autoUpdate: input.checked };
      savePreferences();
      paint();
      if (input.checked) {
        checkForUpdate();
      }
    });
  }

  for (const button of app.querySelectorAll("[data-project]")) {
    button.addEventListener("click", () => {
      if (selected === button.dataset.project) {
        return;
      }
      selected = button.dataset.project;
      view = { status: null, stats: null, logs: { lines: [], following: false }, pending: null };
      paint();
      loadView({ resetLogs: true }).then(paint);
    });
  }

  for (const button of app.querySelectorAll("[data-action]")) {
    button.addEventListener("click", () => {
      const action = button.dataset.action;
      if (action === "retry") {
        button.disabled = true;
        // A retry is a fresh look at the machine as well as the daemon: after a
        // restart the first-run state is what changed.
        loadBootstrap();
        refresh();
        return;
      }
      if (action === "bootstrap") {
        // A click is the consent to the one administrator step. It is the only
        // thing in this window that can reach the elevated command.
        button.disabled = true;
        setupWsl();
        return;
      }
      if (action === "check-update") {
        button.disabled = true;
        checkForUpdate({ manual: true });
        return;
      }
      if (action === "install-update") {
        button.disabled = true;
        installUpdate();
        return;
      }
      if (action === "dismiss-update") {
        update = { ...update, dismissed: true };
        paint();
        return;
      }
      act(action);
    });
  }

  for (const button of app.querySelectorAll("[data-decision]")) {
    button.addEventListener("click", () => {
      decide(button.dataset.decision, button.dataset.approval);
    });
  }

  for (const button of app.querySelectorAll("[data-tab]")) {
    button.addEventListener("click", () => {
      tab = button.dataset.tab === "logs" ? "logs" : "overview";
      view = { ...view, logs: { ...view.logs, following: tab === "logs" } };
      paint();
      if (tab === "logs") {
        followLogs({ reset: view.logs.lines.length === 0 }).then(paint);
      }
    });
  }
}

/**
 * The first-run state. A read: nothing is installed by asking, and a machine
 * that is already set up answers `ready`, which draws no card at all.
 */
async function loadBootstrap() {
  if (!bridge()) {
    return;
  }
  try {
    bootstrap = await call("bootstrap_status");
  } catch (error) {
    bootstrap = { state: "failed", message: `The shell could not check this machine: ${error}` };
  }
}

/**
 * The consent action. This click is the consent: the shell passes it to the one
 * command that may raise a UAC prompt, and reports whatever comes back. Nothing
 * else in the window can reach that command.
 */
async function setupWsl() {
  bootstrap = { ...bootstrap, state: "installing", message: "Waiting for the administrator window…" };
  paint();
  try {
    bootstrap = await call("bootstrap_consent");
  } catch (error) {
    bootstrap = { state: "failed", message: String(error) };
  }
  paint();
}

/** Start or Stop. The daemon decides; the shell only reports. */
async function act(action) {
  const project = selectedProjectName();
  if (!project) {
    return;
  }
  view = { ...view, pending: action, error: null };
  paint();
  try {
    await call("environment_action", { project, action });
  } catch (error) {
    view = { ...view, error: String(error) };
  }
  view = { ...view, pending: null };
  await refresh();
}

async function refresh() {
  status = await askDaemon();
  reconcileSelection();
  if (status.state === "connected") {
    await loadView();
    await loadInbox();
  }
  paint();
}

function startFollowing() {
  if (logsTimer) {
    clearInterval(logsTimer);
  }
  logsTimer = setInterval(() => {
    if (status.state !== "connected" || tab !== "logs") {
      return;
    }
    followLogs().then(paint);
  }, LOGS_INTERVAL_MS);
}

refresh();
setInterval(refresh, POLL_INTERVAL_MS);
startFollowing();
// The first-run state is asked for once and after every consent: it is a probe
// of the machine, not something that changes on a five-second cadence.
loadBootstrap().then(paint);
// The connector details are static for this build; read them once so the MCP
// tab and Settings draw without a second round trip.
loadShellInfo();
// Check once shortly after launch (so the window paints first) and then at a
// slow cadence. `autoUpdate` is the one preference the window keeps locally.
if (preferences.autoUpdate) {
  setTimeout(() => checkForUpdate(), 1500);
  setInterval(() => {
    if (preferences.autoUpdate) {
      checkForUpdate();
    }
  }, UPDATE_INTERVAL_MS);
}

// Wires the shell to the daemon. This file owns the DOM and the polling; all
// rendering lives in render.js. No business logic belongs here: the shell asks
// the daemon (through the Rust commands) and paints what it is told.
//
// Live logs are a cursor, not a second source of truth: the shell asks for
// lines newer than `nextSince` and appends them.

import { render, projectsOf } from "./render.js";

const POLL_INTERVAL_MS = 5000;
const LOGS_INTERVAL_MS = 1500;
const LOG_BUFFER = 500;
const DEFAULT_LOG_TAIL = 100;
const app = document.getElementById("app");

let status = { state: "loading" };
let selected = null;
let tab = "overview";
/** { status, stats, logs: { lines, nextSince, following, error }, pending, error } */
let view = { status: null, stats: null, logs: { lines: [], following: false }, pending: null };
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

function paint() {
  app.dataset.connection = status.state;
  app.innerHTML = render({ ...status, selected, view });

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
        refresh();
        return;
      }
      act(action);
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

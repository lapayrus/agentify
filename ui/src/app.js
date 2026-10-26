// Wires the shell to the daemon. This file owns the DOM and the polling; all
// rendering lives in render.js. No business logic belongs here: the shell asks
// the daemon (through the Rust command) and paints what it is told.

import { render } from "./render.js";

const POLL_INTERVAL_MS = 5000;
const app = document.getElementById("app");

let status = { state: "loading" };

async function askDaemon() {
  const tauri = window.__TAURI__;
  if (!tauri?.core?.invoke) {
    return {
      state: "unreachable",
      reason:
        "This page is not running inside the Agentify desktop shell, so there is no daemon bridge to call.",
    };
  }
  try {
    return await tauri.core.invoke("daemon_status");
  } catch (error) {
    return {
      state: "unreachable",
      reason: `The shell could not ask the daemon: ${error}`,
    };
  }
}

function paint() {
  app.dataset.connection = status.state;
  app.innerHTML = render(status);
  const retry = document.getElementById("retry");
  if (retry) {
    retry.addEventListener("click", () => {
      retry.disabled = true;
      refresh();
    });
  }
}

async function refresh() {
  status = await askDaemon();
  paint();
}

refresh();
setInterval(refresh, POLL_INTERVAL_MS);

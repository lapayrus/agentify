// Pure render functions for the Agentify shell. No DOM access, no fetching:
// app.js owns those. Keeping rendering pure makes every state testable in Node.

export function escapeHtml(value) {
  return String(value ?? "").replace(
    /[&<>"']/g,
    (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[char],
  );
}

function connectionLabel(status) {
  switch (status.state) {
    case "connected":
      return "daemon connected";
    case "unreachable":
      return "daemon unreachable";
    default:
      return "connecting…";
  }
}

function apiLabel(status) {
  if (status.state === "connected") {
    return `api ${status.whoami?.api_version ?? "unknown"}`;
  }
  return "api n/a";
}

function dotClass(status) {
  switch (status.state) {
    case "connected":
      return "dot dot-connected";
    case "unreachable":
      return "dot dot-unreachable";
    default:
      return "dot dot-loading";
  }
}

// The header carries brand plus "N Projects · daemon state · api version".
// The walking skeleton has no Project endpoint yet, so the count is 0.
export function renderHeader(status) {
  return `<header class="top" data-connection="${escapeHtml(status.state)}">
  <div class="brand"><span class="mark">A</span>Agentify</div>
  <div class="daemon">
    <span class="${dotClass(status)}"></span>
    <span class="project-count">0 Projects</span>
    <span class="sep">·</span>
    <span class="connection">${connectionLabel(status)}</span>
    <span class="sep">·</span>
    <span class="api">${apiLabel(status)}</span>
  </div>
</header>`;
}

// The Project rail is empty in the walking skeleton: no Environments exist yet.
export function renderRail() {
  return `<aside class="rail" data-rail="projects" aria-label="Projects">
  <div class="rail-head"><span>Projects</span><span class="badge">0</span></div>
  <p class="rail-empty">No Projects yet</p>
  <p class="rail-hint">Agents create Projects through the MCP server. This shell only shows what the daemon reports.</p>
</aside>`;
}

function renderEmptyProjects(whoami) {
  const scopes = (whoami?.scopes ?? []).join(", ") || "none";
  const capabilities = Object.entries(whoami?.capabilities ?? {})
    .filter(([, enabled]) => enabled)
    .map(([name]) => name);
  return `<main class="main" data-empty="projects" data-state="connected">
  <h1>No Projects yet</h1>
  <p class="lede">This machine has a daemon and no Projects. An Agent creates the first one by calling <code>project.create</code> over the MCP server; it then appears in the rail with its Environment, Resources, and URL.</p>
  <dl class="facts">
    <div><dt>daemon</dt><dd>${escapeHtml(whoami?.daemon_version ?? "unknown")}</dd></div>
    <div><dt>api</dt><dd>${escapeHtml(whoami?.api_version ?? "unknown")}</dd></div>
    <div><dt>scopes</dt><dd>${escapeHtml(scopes)}</dd></div>
    <div><dt>actor</dt><dd>${escapeHtml(whoami?.actor?.id ?? "unknown")}</dd></div>
    <div><dt>instance</dt><dd>${escapeHtml(whoami?.daemon?.instance_id ?? "unknown")}</dd></div>
    <div><dt>capabilities</dt><dd>${escapeHtml(capabilities.join(", ") || "none")}</dd></div>
  </dl>
</main>`;
}

function renderUnreachable(reason) {
  return `<main class="main" data-state="daemon-unreachable">
  <h1>Daemon unreachable</h1>
  <p class="reason">${escapeHtml(reason ?? "The shell could not reach the daemon.")}</p>
  <p class="lede">The desktop shell holds no state of its own: it reads the daemon's discovery file and calls the loopback API. Start the daemon (<code>agentifyd</code>) and retry — this window will pick the new discovery file up automatically.</p>
  <button type="button" class="retry" data-action="retry" id="retry">Retry now</button>
</main>`;
}

function renderLoading() {
  return `<main class="main" data-state="loading">
  <h1>Connecting to the daemon</h1>
  <p class="lede">Reading the discovery file and asking the daemon who it is.</p>
</main>`;
}

export function renderMain(status) {
  if (status.state === "connected") {
    return renderEmptyProjects(status.whoami);
  }
  if (status.state === "unreachable") {
    return renderUnreachable(status.reason);
  }
  return renderLoading();
}

// The three-zone shell: header, Project rail, main pane.
export function render(status) {
  return `<div class="shell" data-connection="${escapeHtml(status.state)}">
${renderHeader(status)}
<div class="body">
${renderRail(status)}
${renderMain(status)}
</div>
</div>`;
}

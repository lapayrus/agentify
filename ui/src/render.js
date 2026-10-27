// Pure render functions for the Agentify shell. No DOM access, no fetching:
// app.js owns those. Keeping rendering pure makes every state testable in Node.
//
// The shell renders what the daemon reports and decides nothing itself: names,
// states, costs, and URLs all come from the API payload.

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

function stateDot(state) {
  const known = ["running", "stopped", "starting", "provisioning", "degraded", "failed", "absent"];
  const safe = known.includes(state) ? state : "unknown";
  return `dot dot-${safe}`;
}

/** The Projects the daemon reported. */
export function projectsOf(status) {
  return Array.isArray(status?.projects) ? status.projects : [];
}

function plural(count, word) {
  return `${count} ${word}${count === 1 ? "" : "s"}`;
}

/** `cpu · memory`, or the em-dash placeholders until stats exist. */
export function costLabel(cost) {
  const cpu = cost?.cpu ?? "—";
  const memory = cost?.memory ?? "—";
  const disk = cost?.disk ?? "—";
  return `${cpu} · ${memory} · ${disk}`;
}

/**
 * The selected Project, if the daemon still lists it. Selection is carried on
 * the status object so the renderer stays pure.
 */
export function selectedProject(status) {
  const projects = projectsOf(status);
  const selected = status?.selected;
  if (selected === undefined || selected === null || selected === "") {
    return undefined;
  }
  return projects.find(
    (project) => project.id === selected || project.name === selected,
  );
}

// The header carries brand plus "N Projects · daemon state · api version".
export function renderHeader(status) {
  const count = projectsOf(status).length;
  return `<header class="top" data-connection="${escapeHtml(status.state)}">
  <div class="brand"><span class="mark">A</span>Agentify</div>
  <div class="daemon">
    <span class="${dotClass(status)}"></span>
    <span class="project-count">${plural(count, "Project")}</span>
    <span class="sep">·</span>
    <span class="connection">${connectionLabel(status)}</span>
    <span class="sep">·</span>
    <span class="api">${apiLabel(status)}</span>
  </div>
</header>`;
}

/**
 * The Project rail. Each entry carries the Project's name, Environment state,
 * and cost placeholders; the selected entry is marked with `aria-selected`.
 */
export function renderRail(status) {
  const projects = projectsOf(status);
  const head = `<div class="rail-head"><span>Projects</span><span class="badge">${projects.length}</span></div>`;

  if (projects.length === 0) {
    const hint =
      status?.state === "unreachable"
        ? "The rail fills in once the daemon answers."
        : "Agents create Projects through the MCP server. This shell only shows what the daemon reports.";
    return `<aside class="rail" data-rail="projects" aria-label="Projects">
  ${head}
  <p class="rail-empty">No Projects yet</p>
  <p class="rail-hint">${hint}</p>
</aside>`;
  }

  const current = selectedProject(status);
  const items = projects
    .map((project) => {
      const state = project.environment?.state ?? "unknown";
      const isSelected = current !== undefined && current.id === project.id;
      return `    <button type="button" class="project" data-project="${escapeHtml(project.id)}" aria-selected="${isSelected}">
      <span class="${stateDot(state)}"></span>
      <span class="project-text">
        <span class="project-name">${escapeHtml(project.name)}</span>
        <span class="project-meta"><span class="project-state">${escapeHtml(state)}</span><span class="sep">·</span><span class="cost">${escapeHtml(costLabel(project.environment?.cost))}</span></span>
      </span>
    </button>`;
    })
    .join("\n");

  return `<aside class="rail" data-rail="projects" aria-label="Projects">
  ${head}
  <nav class="projects">
${items}
  </nav>
</aside>`;
}

function resourceDetail(resource) {
  if (resource.kind === "database") {
    const engine = resource.engine ?? "database";
    const version = resource.version ? ` ${resource.version}` : "";
    const volume = resource.volume ? ` · volume ${resource.volume}` : "";
    return `${engine}${version}${volume}`;
  }
  const image = resource.image ?? (resource.build ? `build ${resource.build.dockerfile}` : "app");
  const command = Array.isArray(resource.command) && resource.command.length > 0
    ? ` · ${resource.command.join(" ")}`
    : "";
  return `${image}${command}`;
}

const STATES = ["running", "stopped", "starting", "provisioning", "degraded", "failed", "absent"];
const HEALTHS = ["healthy", "unhealthy", "unknown"];

function safeState(state) {
  return STATES.includes(state) ? state : "unknown";
}

function safeHealth(health) {
  return HEALTHS.includes(health) ? health : "unknown";
}

function healthDot(health) {
  return `dot dot-health-${safeHealth(health)}`;
}

/**
 * The state a row shows for a Resource: the snapshot's own answer first, and
 * the Environment's state when the daemon has not observed it separately.
 */
function resourceState(resource, fallback) {
  return safeState(resource?.state ?? fallback);
}

function resourceRow(resource, environmentState) {
  const depends = Array.isArray(resource.dependsOn) && resource.dependsOn.length > 0
    ? resource.dependsOn.join(", ")
    : "—";
  const health = resource.health ? `<span class="${healthDot(resource.health)}"></span>${escapeHtml(safeHealth(resource.health))}` : "—";
  return `        <tr data-resource="${escapeHtml(resource.name)}" data-kind="${escapeHtml(resource.kind)}" data-health="${escapeHtml(resource.health ?? "unknown")}">
          <td class="resource-name">${escapeHtml(resource.name)}</td>
          <td>${escapeHtml(resource.kind)}</td>
          <td class="resource-detail">${escapeHtml(resourceDetail(resource))}</td>
          <td>${escapeHtml(resourceState(resource, environmentState))}</td>
          <td>${health}</td>
          <td>${escapeHtml(depends)}</td>
        </tr>`;
}

function renderUrls(project) {
  const routes = Array.isArray(project.routes) ? project.routes : [];
  if (routes.length === 0) {
    return `<section class="urls" data-section="urls">
  <h2>URLs</h2>
  <p class="empty-note">No port is declared, so this Project has no URL yet.</p>
</section>`;
  }
  const items = routes
    .map(
      (route) =>
        `    <li data-route="${escapeHtml(route.resource)}"><code>${escapeHtml(route.url)}</code><span class="route-target">→ ${escapeHtml(route.resource)}</span></li>`,
    )
    .join("\n");
  return `<section class="urls" data-section="urls">
  <h2>URLs</h2>
  <ul class="routes">
${items}
  </ul>
</section>`;
}

/** The detail pane: Environment badge, URLs, and the Resource table. */
export function renderProjectDetail(project, view = {}) {
  const environment = project.environment ?? {};
  const snapshot = view.status ?? null;
  const resources = Array.isArray(snapshot?.resources)
    ? snapshot.resources
    : Array.isArray(project.resources)
      ? project.resources
      : [];
  const state = safeState(snapshot?.environment?.state ?? environment.state ?? "unknown");
  const source =
    project.source === "fixture"
      ? `Created from the bundled <code>${escapeHtml(project.sourceName ?? "fixture")}</code> fixture.`
      : "Created from an inline Manifest.";
  const rows = resources.length
    ? `      <table class="resources">
        <thead>
          <tr><th>Resource</th><th>Kind</th><th>Detail</th><th>State</th><th>Health</th><th>Depends on</th></tr>
        </thead>
        <tbody>
${resources.map((resource) => resourceRow(resource, state)).join("\n")}
        </tbody>
      </table>`
    : `      <p class="empty-note">This Project declares no Resources.</p>`;
  const tab = view.tab === "logs" ? "logs" : "overview";

  return `<main class="main" data-project="${escapeHtml(project.name)}" data-state="connected" data-tab="${tab}">
  <header class="project-head">
    <h1>${escapeHtml(project.name)}</h1>
    <span class="env-badge" data-env="${escapeHtml(environment.name ?? "dev")}">${escapeHtml(environment.name ?? "dev")}</span>
    <span class="state state-${state}"><span class="${stateDot(state)}"></span>${escapeHtml(state)}</span>
  </header>
  <p class="lede">${source} Start brings its Resources up and leases host ports; Stop keeps the runtime and its data.</p>
${renderStatusBar(project, snapshot, view.stats)}
${renderActions(project, snapshot, view.pending)}
${renderTabs(tab)}
${view.error ? `<p class="reason" data-error="environment">${escapeHtml(view.error)}</p>` : ""}
${tab === "logs" ? renderLogs(view.logs) : `<div data-tab-panel="overview">
${renderUrls(project)}
  <section class="resources-section" data-section="resources">
    <h2>Resources</h2>
${rows}
  </section>
  <dl class="facts">
    <div><dt>environment</dt><dd>${escapeHtml(environment.id ?? "—")}</dd></div>
    <div><dt>project</dt><dd>${escapeHtml(project.id ?? "—")}</dd></div>
    <div><dt>workspace</dt><dd>${escapeHtml(project.workspace ?? "—")}</dd></div>
    <div><dt>backend</dt><dd>${escapeHtml(snapshot?.backend?.name ?? "unknown")}</dd></div>
  </dl>
</div>`}
</main>`;
}

/**
 * The status bar: state, health, how many Resources, and cost. Every figure is
 * the daemon's; the shell formats nothing it was not told.
 */
export function renderStatusBar(project, snapshot, stats) {
  const environment = project?.environment ?? {};
  const state = safeState(snapshot?.environment?.state ?? environment.state ?? "unknown");
  const health = safeHealth(snapshot?.health ?? "unknown");
  const resources = Array.isArray(snapshot?.resources)
    ? snapshot.resources
    : Array.isArray(project?.resources)
      ? project.resources
      : [];
  const healthy = resources.filter((resource) => resource.health === "healthy").length;
  const cost = stats ?? environment.cost ?? {};
  const figures = costLabel({ cpu: cost.cpu, memory: cost.memory, disk: cost.disk });

  return `  <div class="statusbar" data-status="${state}" data-health="${health}">
    <span class="status-state"><span class="${stateDot(state)}"></span>${escapeHtml(state)}</span>
    <span class="status-health" data-health="${health}"><span class="${healthDot(health)}"></span>${escapeHtml(health)}</span>
    <span class="status-resources">${escapeHtml(`${resources.length} resource${resources.length === 1 ? "" : "s"}`)}${resources.length > 0 ? ` · ${healthy} healthy` : ""}</span>
    <span class="status-cost cost">${escapeHtml(figures)}</span>
  </div>`;
}

/** Start and Stop. The shell asks the daemon; it never decides the state. */
export function renderActions(project, snapshot, pending) {
  const state = safeState(snapshot?.environment?.state ?? project?.environment?.state ?? "unknown");
  const busy = pending === "start" || pending === "stop";
  const startDisabled = busy || state === "running" || state === "starting";
  const stopDisabled = busy || state === "stopped" || state === "absent";
  return `  <div class="actions">
    <button type="button" class="action" data-action="start"${startDisabled ? " disabled" : ""}>${pending === "start" ? "Starting…" : "Start"}</button>
    <button type="button" class="action" data-action="stop"${stopDisabled ? " disabled" : ""}>${pending === "stop" ? "Stopping…" : "Stop"}</button>
  </div>`;
}

/** The Overview / Logs tabs. */
export function renderTabs(tab) {
  const current = tab === "logs" ? "logs" : "overview";
  return `  <nav class="tabs" role="tablist">
    <button type="button" class="tab" role="tab" data-tab="overview" aria-selected="${current === "overview"}">Overview</button>
    <button type="button" class="tab" role="tab" data-tab="logs" aria-selected="${current === "logs"}">Logs</button>
  </nav>`;
}

/** One log line, escaped, tagged with the Resource and stream it came from. */
function logRow(line) {
  const stream = line?.stream === "stderr" ? "stderr" : "stdout";
  return `    <li class="log-line" data-seq="${escapeHtml(line?.seq ?? "")}" data-resource="${escapeHtml(line?.resource ?? "")}" data-stream="${stream}">
      <span class="log-at">${escapeHtml(line?.at ?? "")}</span>
      <span class="log-resource">${escapeHtml(line?.resource ?? "")}</span>
      <span class="log-text">${escapeHtml(line?.line ?? "")}</span>
    </li>`;
}

/**
 * The Logs tab. `logs` is what the shell has collected: `lines`, a `following`
 * flag, and an optional `error`. Following appends, so the list only grows.
 */
export function renderLogs(logs) {
  const lines = Array.isArray(logs?.lines) ? logs.lines : [];
  const following = logs?.following === true;
  const head = `  <div class="logs-head">
    <span>${escapeHtml(`${lines.length} line${lines.length === 1 ? "" : "s"}`)}</span>
    <span class="live${following ? " live-on" : ""}" data-following="${following}">${following ? "following" : "paused"}</span>
  </div>`;
  if (logs?.error) {
    return `<section class="logs" data-tab-panel="logs">
${head}
  <p class="reason">${escapeHtml(logs.error)}</p>
</section>`;
  }
  if (lines.length === 0) {
    return `<section class="logs" data-tab-panel="logs">
${head}
  <p class="empty-note">No log lines yet. Start the Environment, or run a command in a Resource.</p>
</section>`;
  }
  return `<section class="logs" data-tab-panel="logs">
${head}
  <ol class="log-lines">
${lines.map(logRow).join("\n")}
  </ol>
</section>`;
}

function renderEmptyProjects(whoami) {
  const scopes = (whoami?.scopes ?? []).join(", ") || "none";
  const capabilities = Object.entries(whoami?.capabilities ?? {})
    .filter(([, enabled]) => enabled)
    .map(([name]) => name);
  return `<main class="main" data-empty="projects" data-state="connected">
  <h1>No Projects yet</h1>
  <p class="lede">This machine has a daemon and no Projects. An Agent creates the first one by calling <code>create_project</code> over the MCP server; it then appears in the rail with its Environment, Resources, and URL.</p>
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

function renderProjectsError(reason) {
  return `<main class="main" data-state="connected">
  <h1>Projects unavailable</h1>
  <p class="reason">${escapeHtml(reason)}</p>
  <p class="lede">The daemon answered, but listing Projects failed. The rail is empty because the daemon could not report them, not because none exist.</p>
</main>`;
}

function renderSelectProject() {
  return `<main class="main" data-state="connected" data-selection="none">
  <h1>Select a Project</h1>
  <p class="lede">Pick a Project in the rail to see its Environment, Resources, and URLs.</p>
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
    if (status.projectsError) {
      return renderProjectsError(status.projectsError);
    }
    const projects = projectsOf(status);
    if (projects.length === 0) {
      return renderEmptyProjects(status.whoami);
    }
    const project = selectedProject(status);
    return project
      ? renderProjectDetail(project, status.view ?? {})
      : renderSelectProject();
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

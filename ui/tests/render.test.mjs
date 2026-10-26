// Rendering is pure string production so it can be unit tested in Node without
// a browser. app.js owns the DOM and the daemon polling; everything about what
// a state *looks like* lives here.

import assert from "node:assert/strict";
import test from "node:test";

import {
  renderRail,
  renderHeader,
  renderMain,
  render,
  renderProjectDetail,
  costLabel,
  selectedProject,
} from "../src/render.js";

const whoami = {
  project: null,
  scopes: ["read", "operate"],
  daemon_version: "0.1.0",
  api_version: "v1",
  capabilities: { pty: false, mount: false, checkpoint: false, events: true, resourceStats: false, logSince: false },
  actor: { kind: "agent", id: "token-1" },
  daemon: { instance_id: "instance-1", started_at: "2026-01-01T00:00:00Z" },
};

/** What `daemon_status` returns for a daemon with no Projects. */
const connected = { state: "connected", whoami, projects: [] };

/** The hello-pg payload as the daemon serves it. */
const helloPg = {
  id: "proj-1",
  name: "hello-pg",
  source: "fixture",
  sourceName: "hello-pg",
  createdAt: "2026-01-01T00:00:00Z",
  workspace: "C:\\data\\projects\\proj-1\\workspace",
  environment: {
    id: "env-1",
    name: "dev",
    state: "stopped",
    cost: { cpu: null, memory: null, disk: null },
  },
  routes: [{ resource: "web", hostname: "web", url: "https://web.hello-pg.localhost" }],
  resources: [
    {
      id: "res-db",
      name: "db",
      kind: "database",
      image: null,
      build: null,
      command: [],
      env: {},
      ports: [],
      dependsOn: [],
      healthcheck: null,
      url: null,
      engine: "postgres",
      version: "16",
      volume: "hello-pg-db-data",
      username: "hello",
      state: "stopped",
      cost: { cpu: null, memory: null, disk: null },
    },
    {
      id: "res-web",
      name: "web",
      kind: "app",
      image: null,
      build: { context: ".", dockerfile: "Dockerfile" },
      command: ["node", "server.js"],
      env: { GREETING: "hello" },
      ports: [{ port: 3000, hostname: "web", url: "https://web.hello-pg.localhost" }],
      dependsOn: ["db"],
      healthcheck: { port: 3000, path: "/health", interval: "5s" },
      url: "https://web.hello-pg.localhost",
      engine: null,
      version: null,
      volume: null,
      username: null,
      state: "stopped",
      cost: { cpu: null, memory: null, disk: null },
    },
  ],
};

const secondProject = {
  ...helloPg,
  id: "proj-2",
  name: "inline",
  source: "manifest",
  sourceName: null,
  environment: { id: "env-2", name: "dev", state: "running", cost: { cpu: "3%", memory: "84 MB", disk: null } },
  routes: [{ resource: "web", hostname: "web", url: "https://web.inline.localhost" }],
  resources: [helloPg.resources[1]],
};

const populated = { state: "connected", whoami, projects: [helloPg], selected: "proj-1" };

test("the header names the daemon state, the Project count, and the API version", () => {
  const header = renderHeader(connected);
  assert.match(header, /Agentify/);
  assert.match(header, /0 Projects/);
  assert.match(header, /daemon connected/);
  assert.match(header, /api v1/);

  const withProjects = renderHeader(populated);
  assert.match(withProjects, /1 Project</, "one Project is singular");
  assert.doesNotMatch(withProjects, /1 Projects/);
});

test("the rail lists no Projects and explains who creates them", () => {
  const rail = renderRail(connected);
  assert.match(rail, /Projects/);
  assert.match(rail, /No Projects yet/);
  assert.match(rail, /MCP/);
});

test("the rail lists each Project with its state and cost placeholders", () => {
  const rail = renderRail(populated);
  assert.match(rail, /hello-pg/);
  assert.match(rail, /stopped/);
  assert.match(rail, /—/, "cost is a placeholder until stats exist");
  assert.match(rail, /data-project="proj-1"/);
  assert.match(rail, /aria-selected="true"/, "the selected Project is marked");
  assert.doesNotMatch(rail, /No Projects yet/);

  assert.equal(costLabel({ cpu: null, memory: null, disk: null }), "— · — · —");
  assert.equal(costLabel({ cpu: "3%", memory: "84 MB", disk: null }), "3% · 84 MB · —");
});

test("the rail shows measured cost when the daemon reports it", () => {
  const rail = renderRail({ state: "connected", whoami, projects: [helloPg, secondProject], selected: "proj-2" });
  assert.match(rail, /3% · 84 MB/);
  assert.match(rail, /running/);
  assert.match(rail, /data-project="proj-2"/);
  assert.equal((rail.match(/aria-selected="true"/g) ?? []).length, 1, "exactly one Project is selected");
});

test("the detail pane shows the Environment badge, URLs, and Resources", () => {
  const main = renderMain(populated);
  assert.match(main, /data-project="hello-pg"/);
  assert.match(main, /class="env-badge" data-env="dev">dev</, "the Environment badge reads dev");
  assert.match(main, /https:\/\/web\.hello-pg\.localhost/, "the derived URL is shown");
  assert.match(main, /data-route="web"/);
  assert.match(main, /data-resource="web"/);
  assert.match(main, /data-resource="db"/);
  assert.match(main, /postgres 16 · volume hello-pg-db-data/, "database details are legible");
  assert.match(main, /Dockerfile/);
  assert.match(main, /node server\.js/);
  assert.match(main, /stopped/);
  assert.doesNotMatch(main, /data-empty="projects"/);
});

test("selecting another Project switches the detail pane", () => {
  const main = renderMain({ state: "connected", whoami, projects: [helloPg, secondProject], selected: "proj-2" });
  assert.match(main, /data-project="inline"/);
  assert.doesNotMatch(main, /data-project="hello-pg"/);
  assert.match(main, /https:\/\/web\.inline\.localhost/);
});

test("a Project with no selection asks for one instead of guessing", () => {
  const main = renderMain({ state: "connected", whoami, projects: [helloPg], selected: null });
  assert.match(main, /Select a Project/);
  assert.equal(selectedProject({ projects: [helloPg], selected: null }), undefined);
  assert.equal(selectedProject({ projects: [helloPg], selected: "unknown" }), undefined);
});

test("a connected daemon with no Projects shows the project empty state", () => {
  const main = renderMain(connected);
  assert.match(main, /data-empty="projects"/);
  assert.match(main, /No Projects yet/);
  assert.match(main, /create_project/);
  assert.match(main, /read, operate/);
  assert.match(main, /0\.1\.0/);
  assert.doesNotMatch(main, /Daemon unreachable/);
});

test("a failed Project listing does not pretend there are none", () => {
  const main = renderMain({ state: "connected", whoami, projectsError: "listing Projects failed" });
  assert.match(main, /Projects unavailable/);
  assert.match(main, /not because none exist/);
  assert.doesNotMatch(main, /data-empty="projects"/);

  const rail = renderRail({ state: "connected", whoami, projectsError: "listing Projects failed" });
  assert.match(rail, /No Projects yet/);
});

test("an unreachable daemon shows a distinct connection state, not the empty state", () => {
  const unreachable = { state: "unreachable", reason: "daemon unreachable: no discovery file at C:\\data\\discovery.json" };
  const main = renderMain(unreachable);
  assert.match(main, /data-state="daemon-unreachable"/);
  assert.match(main, /Daemon unreachable/);
  assert.match(main, /no discovery file/);
  assert.match(main, /data-action="retry"/);
  assert.doesNotMatch(main, /data-empty="projects"/);

  const header = renderHeader(unreachable);
  assert.match(header, /daemon unreachable/);
  assert.doesNotMatch(header, /daemon connected/);
});

test("the loading state is neither connected nor an error", () => {
  const header = renderHeader({ state: "loading" });
  assert.match(header, /connecting/);
  assert.match(renderMain({ state: "loading" }), /data-state="loading"/);
});

test("daemon-supplied text is escaped, never injected", () => {
  const hostile = { state: "unreachable", reason: '<img src=x onerror="alert(1)">' };
  const main = renderMain(hostile);
  assert.doesNotMatch(main, /<img/);
  assert.match(main, /&lt;img/);

  const hostileProject = {
    ...helloPg,
    name: '<script>alert("pwned")</script>',
    routes: [{ resource: "web", hostname: "web", url: '<img src=x onerror="alert(1)">' }],
  };
  const detail = renderProjectDetail(hostileProject);
  assert.doesNotMatch(detail, /<script>/);
  assert.match(detail, /&lt;script&gt;/);
  assert.doesNotMatch(detail, /<img/);

  const rail = renderRail({ state: "connected", whoami, projects: [hostileProject], selected: hostileProject.id });
  assert.doesNotMatch(rail, /<script>/, "the rail escapes Project names too");
});

test("render composes header, rail, and main into the three-zone shell", () => {
  const html = render(populated);
  assert.match(html, /<header/);
  assert.match(html, /<aside/);
  assert.match(html, /<main/);
  assert.ok(html.indexOf("<header") < html.indexOf("<aside"));
  assert.ok(html.indexOf("<aside") < html.indexOf("<main"));

  const empty = render(connected);
  assert.match(empty, /<header/);
  assert.match(empty, /No Projects yet/);
});

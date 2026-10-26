// Rendering is pure string production so it can be unit tested in Node without
// a browser. app.js owns the DOM and the daemon polling; everything about what
// a state *looks like* lives here.

import assert from "node:assert/strict";
import test from "node:test";

import { renderRail, renderHeader, renderMain, render } from "../src/render.js";

const connected = {
  state: "connected",
  whoami: {
    project: null,
    scopes: ["read", "operate"],
    daemon_version: "0.1.0",
    api_version: "v1",
    capabilities: { pty: false, mount: false, checkpoint: false, events: true, resourceStats: false, logSince: false },
    actor: { kind: "agent", id: "token-1" },
    daemon: { instance_id: "instance-1", started_at: "2026-01-01T00:00:00Z" },
  },
};

test("the header names the daemon state and the API version", () => {
  const header = renderHeader(connected);
  assert.match(header, /Agentify/);
  assert.match(header, /0 Projects/);
  assert.match(header, /daemon connected/);
  assert.match(header, /api v1/);
});

test("the rail lists no Projects and explains who creates them", () => {
  const rail = renderRail(connected);
  assert.match(rail, /Projects/);
  assert.match(rail, /No Projects yet/);
  assert.match(rail, /MCP/);
});

test("a connected daemon with no Projects shows the project empty state", () => {
  const main = renderMain(connected);
  assert.match(main, /data-empty="projects"/);
  assert.match(main, /No Projects yet/);
  assert.match(main, /project\.create/);
  assert.match(main, /read, operate/);
  assert.match(main, /0\.1\.0/);
  assert.doesNotMatch(main, /Daemon unreachable/);
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
});

test("render composes header, rail, and main into the three-zone shell", () => {
  const html = render(connected);
  assert.match(html, /<header/);
  assert.match(html, /<aside/);
  assert.match(html, /<main/);
  assert.ok(html.indexOf("<header") < html.indexOf("<aside"));
  assert.ok(html.indexOf("<aside") < html.indexOf("<main"));
});

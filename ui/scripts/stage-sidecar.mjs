// Stages the workspace binaries Tauri ships as sidecars.
//
// Tauri copies each `bundle.externalBin` entry into the install directory beside
// the app and strips the target triple from the name, so the daemon ends up at
// `<install dir>\agentifyd.exe` — exactly where `login_item::daemon_binary()`
// looks for it and where the Scheduled Task points.
//
// The staged files are build outputs: `ui/src-tauri/binaries/` is gitignored. A
// release build is the default because that is what an installer ships; pass
// `--debug` (or set AGENTIFY_SIDECAR_PROFILE=debug) to stage debug binaries.
//
// Caddy is not staged here: the installer stages it on first run into
// `%LOCALAPPDATA%\Agentify\tools\caddy.exe`, outside the program directory, so
// installing over a version does not discard a downloaded binary.

import { execFileSync } from "node:child_process";
import { copyFile, mkdir, readFile, stat } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const uiRoot = path.resolve(here, "..");
const repoRoot = path.resolve(uiRoot, "..");
const tauriRoot = path.join(uiRoot, "src-tauri");
const binaries = path.join(tauriRoot, "binaries");

/** Package names in the workspace and the binary each one builds. */
const SIDECARS = [
  { pkg: "agentify-daemon", bin: "agentifyd" },
  { pkg: "agentify-mcp", bin: "agentify-mcp" },
  { pkg: "agentify-cli", bin: "agentify" },
];

const profile =
  process.argv.includes("--debug") || process.env.AGENTIFY_SIDECAR_PROFILE === "debug"
    ? "debug"
    : "release";

/** The host target triple: what Tauri expects in `binaries/<name>-<triple>.exe`. */
function hostTriple() {
  const out = execFileSync("rustc", ["-vV"], { cwd: repoRoot, encoding: "utf8" });
  const line = out.split(/\r?\n/).find((row) => row.startsWith("host: "));
  if (!line) {
    console.error("stage-sidecar: `rustc -vV` did not report a host triple");
    process.exit(1);
  }
  return line.slice("host: ".length).trim();
}

const triple = hostTriple();
const exeSuffix = triple.includes("windows") ? ".exe" : "";
const targetDir = process.env.CARGO_TARGET_DIR
  ? path.resolve(repoRoot, process.env.CARGO_TARGET_DIR)
  : path.join(repoRoot, "target");

const args = ["build", "--profile", profile];
for (const { pkg } of SIDECARS) {
  args.push("-p", pkg);
}

console.log(`stage-sidecar: cargo ${args.join(" ")}`);
execFileSync("cargo", args, { cwd: repoRoot, stdio: "inherit" });

await mkdir(binaries, { recursive: true });
for (const { bin } of SIDECARS) {
  const from = path.join(targetDir, profile, `${bin}${exeSuffix}`);
  const to = path.join(binaries, `${bin}-${triple}${exeSuffix}`);
  await stat(from).catch(() => {
    console.error(`stage-sidecar: expected ${from}; the build did not produce it`);
    process.exit(1);
  });
  await copyFile(from, to);
  const { size } = await stat(to);
  console.log(`stage-sidecar: ${path.relative(repoRoot, to)} (${size} bytes)`);
}

// The daemon's version is what the handshake pins; the installer's version must
// agree with it or an installed app would refuse its own sidecar.
const [daemonVersion, appVersion] = await Promise.all([
  readFile(path.join(repoRoot, "Cargo.toml"), "utf8").then(workspaceVersion),
  readFile(path.join(tauriRoot, "tauri.conf.json"), "utf8").then(
    (text) => JSON.parse(text).version,
  ),
]);
if (daemonVersion !== appVersion) {
  console.error(
    `stage-sidecar: the workspace is ${daemonVersion} and tauri.conf.json is ${appVersion}; ` +
      "the installer and the daemon it ships must be one version",
  );
  process.exit(1);
}
console.log(`stage-sidecar: version ${appVersion} agrees with the workspace`);

/** The `version = "…"` line under `[workspace.package]`. */
function workspaceVersion(toml) {
  const section = toml.split(/^\[/m).find((block) => block.startsWith("workspace.package]"));
  const match = section?.match(/^version\s*=\s*"([^"]+)"/m);
  if (!match) {
    console.error("stage-sidecar: no version under [workspace.package]");
    process.exit(1);
  }
  return match[1];
}

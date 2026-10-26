// Copies the static front end into ui/dist, which is what Tauri embeds.
// No bundler: the shell is plain HTML/CSS/JS modules.

import { cp, mkdir, readdir, rm } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const uiRoot = path.resolve(here, "..");
const src = path.join(uiRoot, "src");
const dist = path.join(uiRoot, "dist");

const REQUIRED = ["index.html", "app.js", "render.js", "styles.css"];

await rm(dist, { recursive: true, force: true });
await mkdir(dist, { recursive: true });
await cp(src, dist, { recursive: true });

const files = await readdir(dist);
const missing = REQUIRED.filter((name) => !files.includes(name));
if (missing.length > 0) {
  console.error(`build: missing ${missing.join(", ")} in ${dist}`);
  process.exit(1);
}

console.log(`build: ${files.length} files in ${path.relative(uiRoot, dist)} (${files.join(", ")})`);

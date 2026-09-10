#!/usr/bin/env node
// A package whose real payload is a native binary ships one npm package per platform and
// lists them all as `optionalDependencies`; npm and pnpm then install the one that matches.
// @tauri-apps/cli, esbuild and rollup are all built that way. If the lockfile records the
// parent without its optional list, `pnpm install --frozen-lockfile` on a fresh clone
// installs a JS shim with no binary behind it and `pnpm tauri` dies at require() time --
// while the machine that produced the lockfile keeps working, because its store still holds
// the platform package from an earlier install. That is exactly how 9a181cb shipped: a
// lockfile "refresh" dropped all eleven @tauri-apps/cli-* entries and nothing noticed for
// two days.
//
// So: for every package actually installed under node_modules/.pnpm, read its own
// package.json, and assert that every name it declares in optionalDependencies is recorded
// in the lockfile snapshot for that package. Runs after `pnpm install --frozen-lockfile`.

import { readFileSync, readdirSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const virtualStore = join(root, "node_modules", ".pnpm");

// Optional deps a registry cannot serve (yanked, or published for a platform the parent
// still lists). Each entry is "<parent name>@<version> -> <optional dep name>" and needs a
// reason, because the alternative reading of a missing entry is the bug above.
const KNOWN_ABSENT = new Set([]);

if (!existsSync(virtualStore)) {
  console.error(
    "check-lockfile: node_modules/.pnpm is missing. Run `pnpm install --frozen-lockfile` first."
  );
  process.exit(1);
}

// pnpm writes this file itself, so its shape is fixed: two-space entries under a
// column-zero section header, four-space keys inside an entry, six-space leaf values.
// That is enough structure to read without a YAML dependency.
function readSnapshotOptionalDeps(lockfileText) {
  const byId = new Map();
  let inSnapshots = false;
  let current = null;
  let inOptional = false;

  for (const raw of lockfileText.split("\n")) {
    const line = raw.replace(/\r$/, "");
    if (line.trim() === "" || line.trimStart().startsWith("#")) continue;

    if (!/^\s/.test(line)) {
      inSnapshots = line.startsWith("snapshots:");
      current = null;
      inOptional = false;
      continue;
    }
    if (!inSnapshots) continue;

    const indent = line.length - line.trimStart().length;
    if (indent === 2) {
      const key = line.trim().replace(/:( \{\})?$/, "");
      current = key.replace(/^'|'$/g, "");
      byId.set(current, new Set());
      inOptional = false;
    } else if (indent === 4 && current !== null) {
      inOptional = line.trim() === "optionalDependencies:";
    } else if (indent === 6 && inOptional && current !== null) {
      const name = line.trim().split(":")[0].replace(/^'|'$/g, "");
      byId.get(current).add(name);
    }
  }
  return byId;
}

// "@tauri-apps+cli@2.6.2" -> { name: "@tauri-apps/cli", version: "2.6.2" }, and the peer
// suffix pnpm appends for a package resolved against several peer sets is dropped: the
// optional list does not vary with peers.
function parseStoreDir(dir) {
  const withoutPeers = dir.replace(/\(.*$/, "").replace(/_[a-f0-9]{40,}$/, "");
  const at = withoutPeers.lastIndexOf("@");
  if (at <= 0) return null;
  const name = withoutPeers.slice(0, at).replace("+", "/");
  const version = withoutPeers.slice(at + 1);
  if (!/^\d/.test(version)) return null;
  return { name, version };
}

const snapshots = readSnapshotOptionalDeps(readFileSync(join(root, "pnpm-lock.yaml"), "utf8"));

// A snapshot id carries the same peer suffix the store dir does; index by the bare
// name@version so a lookup does not have to reproduce pnpm's suffix spelling.
const snapshotsByBareId = new Map();
for (const [id, optional] of snapshots) {
  const bare = id.replace(/\(.*$/, "");
  const merged = snapshotsByBareId.get(bare);
  if (merged) for (const n of optional) merged.add(n);
  else snapshotsByBareId.set(bare, new Set(optional));
}

const problems = [];
let checked = 0;

for (const dir of readdirSync(virtualStore)) {
  if (dir === "node_modules" || dir === "lock.yaml") continue;
  const parsed = parseStoreDir(dir);
  if (!parsed) continue;

  const manifestPath = join(virtualStore, dir, "node_modules", ...parsed.name.split("/"), "package.json");
  if (!existsSync(manifestPath)) continue;

  let manifest;
  try {
    manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  } catch {
    continue;
  }
  const declared = Object.keys(manifest.optionalDependencies ?? {});
  if (declared.length === 0) continue;

  checked += 1;
  const bareId = `${parsed.name}@${parsed.version}`;
  const recorded = snapshotsByBareId.get(bareId);
  if (!recorded) {
    problems.push({ parent: bareId, missing: declared, reason: "no snapshot entry at all" });
    continue;
  }
  const missing = declared.filter(
    name => !recorded.has(name) && !KNOWN_ABSENT.has(`${bareId} -> ${name}`)
  );
  if (missing.length > 0) problems.push({ parent: bareId, missing, reason: "not in the snapshot" });
}

if (problems.length > 0) {
  console.error("pnpm-lock.yaml is missing optional dependencies that packages declare:\n");
  for (const { parent, missing, reason } of problems) {
    console.error(`  ${parent} (${reason})`);
    for (const name of missing) console.error(`      ${name}`);
  }
  console.error(
    "\nA lockfile in this state installs the parent without its native binary on any" +
      "\nplatform whose package is absent, and only on a machine whose pnpm store already" +
      "\nholds it does anything still work. Fix it with:" +
      "\n\n    pnpm install --lockfile-only --fix-lockfile" +
      "\n\nand check the diff is purely additive. Never `pnpm install --no-optional`."
  );
  process.exit(1);
}

console.log(
  `check-lockfile: ${checked} package${checked === 1 ? "" : "s"} declare optionalDependencies; ` +
    "all are recorded in pnpm-lock.yaml."
);

#!/usr/bin/env node
// A package whose payload is a native binary (@tauri-apps/cli, esbuild, rollup) ships one npm
// package per platform, listed as `optionalDependencies`. If the lockfile records the parent
// without that list, `pnpm install --frozen-lockfile` on a fresh clone installs a JS shim with
// no binary and `pnpm tauri` dies at require(), while the machine that wrote the lockfile
// keeps working off its store.
//
// Checks every package installed under node_modules/.pnpm: each name its package.json declares
// in optionalDependencies must be in the lockfile snapshot for that package. Run after
// `pnpm install --frozen-lockfile`.

import { readFileSync, readdirSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const virtualStore = join(root, "node_modules", ".pnpm");

// optional deps a registry cannot serve (yanked, or published for a platform the parent
// still lists). Each entry is "<parent name>@<version> -> <optional dep name>" and needs a reason
const KNOWN_ABSENT = new Set([]);

if (!existsSync(virtualStore)) {
  console.error(
    "check-lockfile: node_modules/.pnpm is missing. Run `pnpm install --frozen-lockfile` first."
  );
  process.exit(1);
}

// pnpm writes the lockfile, so its shape is fixed: two-space entries under a column-zero
// section header, four-space keys inside an entry, six-space leaf values. No YAML parser needed
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

// "@tauri-apps+cli@2.6.2" -> { name: "@tauri-apps/cli", version: "2.6.2" }. pnpm's peer suffix
// is dropped: the optional list does not vary with peers
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

// snapshot ids carry the peer suffix too; index by bare name@version so lookups need not
// reproduce it
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

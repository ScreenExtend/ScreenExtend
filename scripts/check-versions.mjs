#!/usr/bin/env node
// The app version is written out in four places and they have to agree: an installer built
// from a tree where they disagree ships a bundle whose CFBundleVersion, Cargo metadata and
// updater manifest name different releases, and `finalize-release` renames assets by the
// version it reads from tauri.conf.json alone.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = p => readFileSync(join(root, p), "utf8");

const sources = [
  ["package.json", JSON.parse(read("package.json")).version],
  ["src-tauri/tauri.conf.json", JSON.parse(read("src-tauri/tauri.conf.json")).version],
  [
    "src-tauri/tauri.macos.conf.json",
    JSON.parse(read("src-tauri/tauri.macos.conf.json")).version,
  ],
  [
    "src-tauri/Cargo.toml",
    (read("src-tauri/Cargo.toml").match(/^version\s*=\s*"([^"]+)"/m) ?? [])[1],
  ],
];

const missing = sources.filter(([, v]) => !v);
if (missing.length > 0) {
  console.error("check-versions: no version found in " + missing.map(([f]) => f).join(", "));
  process.exit(1);
}

const distinct = new Set(sources.map(([, v]) => v));
if (distinct.size !== 1) {
  console.error("the app version differs between files:\n");
  for (const [file, version] of sources) console.error(`  ${version}\t${file}`);
  console.error("\nBump all four together.");
  process.exit(1);
}

console.log(`version ${sources[0][1]} is consistent across all four files.`);

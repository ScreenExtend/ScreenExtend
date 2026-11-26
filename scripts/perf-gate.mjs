#!/usr/bin/env node
// Compare the timing numbers a measurement run printed against ci/perf-baseline.json
// and fail when one of them has regressed beyond the tolerance.
//
//   node scripts/perf-gate.mjs --key <baseline-key> [--input <file>] [options]
//   cargo test --release --lib x264_encoder -- --nocapture | node scripts/perf-gate.mjs --key ci-windows
//
// Options:
//   --key <k>        which entry of `baselines` to compare against. Required.
//   --input <file>   read the measurement text from a file instead of stdin. Repeatable:
//                    with several runs the LOWEST value per metric is used, the one closest
//                    to the uncontended cost (one x264 p95 ranged 4405-9067 us across runs
//                    on a busy machine).
//   --baseline <f>   default ci/perf-baseline.json.
//   --out <file>     write the observed numbers as JSON (a CI artifact; also what you
//                    paste back into the baseline file to re-seed a runner).
//   --seed           update the baseline file in place with the observed numbers
//                    instead of comparing. Never run this from a gating job.
//   --tolerance <f>  override the tolerance, as a fraction (0.05 = 5%). Otherwise a key's own
//                    `tolerance` is used if it has one, else the file-wide default. Per key,
//                    because an idle dev box and a shared CI VM are not the same instrument.
//
// Exit 0 = within tolerance, or nothing to compare against yet; 1 = regression;
// 2 = usage/parse error.
//
// The metric regexes live here because they match the exact strings the Rust side prints;
// the numbers are per machine and live in JSON, so a runner is re-seeded without a code change.

import { readFileSync, writeFileSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

// `source` is what to run to reproduce a failure. The capture group is already in `unit`:
// nothing is rescaled.
const METRICS = {
  x264_p95_encode_us: {
    // `x264 smoke OK: ... p95_encode=4537us (budget 16666us)`, from the x264 unit test.
    // Anchored on the prefix: other harnesses print a `p95_encode=` of their own.
    pattern: /x264 smoke OK:[^\n]*p95_encode=(\d+)us/,
    unit: "us",
    source: "cargo test --release --lib x264_encoder -- --nocapture",
  },
  x264_wall_ms: {
    pattern: /x264 smoke OK:[^\n]*wall=(\d+)ms/,
    unit: "ms",
    source: "cargo test --release --lib x264_encoder -- --nocapture",
  },
  probe_encode_p99_ms: {
    // `telemetry: ... encode=<p50>/<p90>/<p99>/<max>`, from any probe run that encodes
    pattern: /\bencode=[\d.]+\/[\d.]+\/([\d.]+)\/[\d.]+/,
    unit: "ms",
    source: "ScreenExtend probe --encode",
  },
  probe_capture_to_socket_p99_ms: {
    pattern: /\bcapture_to_socket=[\d.]+\/[\d.]+\/([\d.]+)\/[\d.]+/,
    unit: "ms",
    source: "ScreenExtend probe --whep-selftest --duration 30",
  },
  // `windows capture->encoded OK: ... capture_to_encoded=<p50>/<p90>/<p99>/<max> (p50/p90/p99/max ms)`
  // The only metric spanning capture as well as encode: an encode-side win that costs a
  // capture-side stall shows up only here. Needs an interactive desktop, so no CI runner has it.
  windows_capture_to_encoded_p99_ms: {
    pattern: /\bcapture_to_encoded=[\d.]+\/[\d.]+\/([\d.]+)\/[\d.]+/,
    unit: "ms",
    source:
      "cargo test --release --lib windows_capture_to_encoded -- --ignored --nocapture",
  },
  windows_capture_to_encoded_p50_ms: {
    pattern: /\bcapture_to_encoded=([\d.]+)\/[\d.]+\/[\d.]+\/[\d.]+/,
    unit: "ms",
    source:
      "cargo test --release --lib windows_capture_to_encoded -- --ignored --nocapture",
  },
  // Encode stage of the same run. Not named like the x264 metric on purpose: this is whichever
  // encoder the `Auto` ladder picked, so it must not be compared against an x264 baseline.
  windows_capture_encode_p95_us: {
    pattern: /\bcapture_encode_p95=(\d+)us/,
    unit: "us",
    source:
      "cargo test --release --lib windows_capture_to_encoded -- --ignored --nocapture",
  },
  // `videotoolbox encode OK: ... vt_encode_p95=<n>us ...`, from macOS's `encode_synthetic`.
  // Needs no display and no Screen Recording grant, so a hosted runner can measure it.
  vt_p95_encode_us: {
    pattern: /\bvt_encode_p95=(\d+)us/,
    unit: "us",
    source:
      "cargo test --release --lib macos_utils::streamer::encoder -- --ignored --nocapture",
  },
  vt_p50_encode_us: {
    pattern: /\bvt_encode_p50=(\d+)us/,
    unit: "us",
    source:
      "cargo test --release --lib macos_utils::streamer::encoder -- --ignored --nocapture",
  },
};

function die(msg) {
  console.error(`perf-gate: ${msg}`);
  process.exit(2);
}

// the header comment is the help text: read it back, stopping at the first non-comment line
function headerComment() {
  const lines = readFileSync(fileURLToPath(import.meta.url), "utf8").split("\n").slice(1);
  const end = lines.findIndex((l) => !l.startsWith("//"));
  return lines.slice(0, end === -1 ? lines.length : end).join("\n");
}

function parseArgs(argv) {
  const out = { baseline: resolve(REPO_ROOT, "ci/perf-baseline.json"), inputs: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => {
      const v = argv[++i];
      if (v === undefined) die(`${a} needs a value`);
      return v;
    };
    switch (a) {
      case "--key":
        out.key = next();
        break;
      case "--input":
        out.inputs.push(next());
        break;
      case "--baseline":
        out.baseline = resolve(next());
        break;
      case "--out":
        out.out = resolve(next());
        break;
      case "--tolerance":
        out.tolerance = Number(next());
        break;
      case "--seed":
        out.seed = true;
        break;
      case "-h":
      case "--help":
        console.log(headerComment());
        process.exit(0);
        break;
      default:
        die(`unknown option ${a}`);
    }
  }
  return out;
}

function readRuns(paths) {
  if (paths.length > 0) return paths.map((p) => readFileSync(resolve(p), "utf8"));
  try {
    return [readFileSync(0, "utf8")];
  } catch {
    die("no --input given and stdin is empty");
  }
}

const args = parseArgs(process.argv.slice(2));
if (!args.key) die("--key is required (an entry of `baselines` in the baseline file)");

let file;
try {
  file = JSON.parse(readFileSync(args.baseline, "utf8"));
} catch (e) {
  die(`cannot read ${args.baseline}: ${e.message}`);
}

// resolved after the baseline entry is read, so a key can override the default
function resolveTolerance(baselineEntry) {
  const t = args.tolerance ?? baselineEntry?.tolerance ?? file.tolerance;
  if (!Number.isFinite(t)) die("no usable tolerance (set `tolerance` in the baseline file)");
  return t;
}
const tolerance = resolveTolerance(file.baselines?.[args.key]);

const runs = readRuns(args.inputs);

// Best of N: the lowest reading is the least contended. A mean or median would fold the
// machine's other work into the number.
const observed = {};
const samples = {};
for (const text of runs) {
  for (const [name, def] of Object.entries(METRICS)) {
    const m = def.pattern.exec(text);
    if (!m) continue;
    const value = Number(m[1]);
    (samples[name] ??= []).push(value);
    if (!(name in observed) || value < observed[name]) observed[name] = value;
  }
}

if (Object.keys(observed).length === 0) {
  console.error("perf-gate: the measurement output contained none of the known metrics:");
  for (const [name, def] of Object.entries(METRICS)) console.error(`  ${name}  <- ${def.source}`);
  process.exit(2);
}

console.log(`perf-gate: key=${args.key} tolerance=${(tolerance * 100).toFixed(1)}% runs=${runs.length}`);
for (const [name, value] of Object.entries(observed)) {
  const all = samples[name];
  const spread = all.length > 1 ? ` (best of ${all.join(", ")})` : "";
  console.log(`  observed  ${name} = ${value}${METRICS[name].unit}${spread}`);
}

if (args.out) {
  writeFileSync(args.out, `${JSON.stringify({ key: args.key, observed }, null, 2)}\n`);
  console.log(`perf-gate: wrote ${args.out}`);
}

if (args.seed) {
  file.baselines ??= {};
  file.baselines[args.key] = {
    ...(file.baselines[args.key] ?? {}),
    ...observed,
    seeded: new Date().toISOString().slice(0, 10),
  };
  writeFileSync(args.baseline, `${JSON.stringify(file, null, 2)}\n`);
  console.log(`perf-gate: seeded ${args.key} into ${args.baseline}`);
  process.exit(0);
}

const baseline = file.baselines?.[args.key];
if (!baseline) {
  // An unseeded runner is not a regression: report and pass. The --out artifact is what
  // gets committed to turn this into a gate.
  console.log(`perf-gate: no baseline for '${args.key}' yet, nothing to compare.`);
  console.log("perf-gate: to make this a gate, add the observed numbers above to");
  console.log(`perf-gate:   ${args.baseline} under baselines['${args.key}'].`);
  process.exit(0);
}

let regressed = 0;
let compared = 0;
for (const [name, value] of Object.entries(observed)) {
  const want = baseline[name];
  if (!Number.isFinite(want)) {
    console.log(`  skip      ${name} (no baseline value)`);
    continue;
  }
  compared++;
  const ceiling = want * (1 + tolerance);
  const delta = ((value - want) / want) * 100;
  const sign = delta >= 0 ? "+" : "";
  if (value > ceiling) {
    regressed++;
    console.log(
      `  REGRESS   ${name} = ${value}${METRICS[name].unit} vs baseline ${want} ` +
        `(${sign}${delta.toFixed(1)}%, ceiling ${ceiling.toFixed(0)})`,
    );
    console.log(`            reproduce with: ${METRICS[name].source}`);
  } else {
    console.log(
      `  ok        ${name} = ${value}${METRICS[name].unit} vs baseline ${want} (${sign}${delta.toFixed(1)}%)`,
    );
  }
}

if (compared === 0) {
  console.log(`perf-gate: baseline '${args.key}' has no value for any observed metric, nothing gated.`);
  process.exit(0);
}

if (regressed > 0) {
  console.error(`perf-gate: FAIL: ${regressed} metric(s) beyond the ${(tolerance * 100).toFixed(1)}% tolerance.`);
  process.exit(1);
}
console.log(`perf-gate: PASS: ${compared} metric(s) within tolerance.`);

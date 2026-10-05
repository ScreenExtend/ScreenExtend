#!/usr/bin/env node
// Runs every client-page harness in one go: `pnpm test:client`.
//
// The client page has no build step and no dependencies, and that constraint was read as also
// excluding a test runner. It does not have to — these harnesses need nothing but Node, load
// the shipped `.js` files with `vm` and the smallest possible browser stubs, and leave
// `src-tauri/src/streamer/static/` byte-identical.
//
// Each harness exits non-zero on a failed assertion; this exits non-zero if any of them did,
// after running all of them, so one failure does not hide the rest.
import { spawnSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const STATIC = process.argv[2];

const HARNESSES = [
  ['input.js wire format + telemetry', 'test-client-stats.mjs'],
  ['input.js golden vectors, cross-checked by the Rust parser', 'test-client-input.mjs'],
  ['audio-worker.js decode, redundancy and the sequence gate', 'test-client-audio.mjs'],
  ['audio-worklet.js jitter buffer and the planar ring ABI', 'test-client-worklet.mjs'],
  ['transform-worker.js WebCodecs video path and A/V sync', 'test-client-transform.mjs'],
  ['index.html ICE gathering', 'test-client-ice.mjs'],
  ['index.html encoded transform installed before Chrome short-circuits it', 'test-client-transform-start.mjs'],
  ['index.html /reconfig epochs and kick', 'test-client-reconfig.mjs'],
  ['index.html join path: setup failures and the load-time warm-up', 'test-client-join.mjs'],
];

let failed = 0;
for (const [what, file] of HARNESSES) {
  console.log('\n=== ' + what + '  (' + file + ') ===');
  const args = [resolve(HERE, file)];
  if (STATIC) args.push(STATIC);
  const r = spawnSync(process.execPath, args, { stdio: 'inherit' });
  if (r.status !== 0) {
    failed++;
    console.log('--- ' + file + ' exited ' + r.status);
  }
}

console.log('');
if (failed) {
  console.error(failed + ' of ' + HARNESSES.length + ' client harness(es) failed');
  process.exit(1);
}
console.log('all ' + HARNESSES.length + ' client harnesses passed');

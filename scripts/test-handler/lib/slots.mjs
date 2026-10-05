// Machine-wide coordination between handler processes, all through files under
// %TEMP%\screenextend-test-handler\:
//   slot-<n>.json     one per running test (n = 1..4), created with O_EXCL, holds the owner pid
//   exclusive.json    an exclusive-mode caller's intent: no new slot is handed out while it exists
//   measure.lock      the measurement lock: held across every timed window and every audio test
// A file whose pid is no longer alive is stale and is removed by whoever finds it.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

export const ROOT = path.join(os.tmpdir(), 'screenextend-test-handler');
export const MAX_SLOTS = 4;
fs.mkdirSync(ROOT, { recursive: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export function pidAlive(pid) {
  if (!pid) return false;
  try { process.kill(pid, 0); return true; } catch (e) { return e.code === 'EPERM'; }
}

function readOwner(file) {
  try { return JSON.parse(fs.readFileSync(file, 'utf8')); } catch { return null; }
}

/** Try to create `file` exclusively; clear it first when its owner is dead. */
function tryTake(file, info) {
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      const fd = fs.openSync(file, 'wx');
      fs.writeSync(fd, JSON.stringify({ pid: process.pid, since: new Date().toISOString(), ...info }));
      fs.closeSync(fd);
      return true;
    } catch (e) {
      if (e.code !== 'EEXIST') throw e;
      const o = readOwner(file);
      // A file caught mid-write reads as null; give its writer a moment before calling it stale.
      if (o && pidAlive(o.pid)) return false;
      if (!o) { try { if (Date.now() - fs.statSync(file).mtimeMs < 3000) return false; } catch { continue; } }
      try { fs.rmSync(file, { force: true }); } catch { return false; }
    }
  }
  return false;
}

function release(file) {
  const o = readOwner(file);
  if (o && o.pid === process.pid) fs.rmSync(file, { force: true });
}

const slotFile = (n) => path.join(ROOT, `slot-${n}.json`);
const EXCL = path.join(ROOT, 'exclusive.json');
const MEASURE = path.join(ROOT, 'measure.lock');

export function slotStatus() {
  const out = [];
  for (let n = 1; n <= MAX_SLOTS; n++) { const o = readOwner(slotFile(n)); out.push(o && pidAlive(o.pid) ? { slot: n, ...o } : { slot: n, free: true }); }
  const ex = readOwner(EXCL); const m = readOwner(MEASURE);
  return { slots: out, exclusive: ex && pidAlive(ex.pid) ? ex : null, measure: m && pidAlive(m.pid) ? m : null };
}

/** Wait for a free slot. Returns { slot, waitedMs }. */
export async function acquireSlot(info, log) {
  const t0 = Date.now(); let said = false;
  for (;;) {
    const ex = readOwner(EXCL);
    const exclusiveWaiting = ex && pidAlive(ex.pid) && ex.pid !== process.pid;
    if (!exclusiveWaiting) {
      for (let n = 1; n <= MAX_SLOTS; n++) if (tryTake(slotFile(n), info)) return { slot: n, waitedMs: Date.now() - t0 };
    }
    if (!said) { log(`queued: all ${MAX_SLOTS} slots busy${exclusiveWaiting ? ' or an exclusive run is waiting' : ''}`); said = true; }
    await sleep(500 + Math.random() * 300);
  }
}

/** Exclusive mode: announce, then take every slot. Returns { slots, waitedMs }. */
export async function acquireAll(info, log) {
  const t0 = Date.now(); let said = false;
  while (!tryTake(EXCL, info)) { if (!said) { log('queued behind another exclusive run'); said = true; } await sleep(700); }
  const held = [];
  for (let n = 1; n <= MAX_SLOTS; n++) {
    while (!tryTake(slotFile(n), { ...info, exclusive: true })) { if (!said) { log('exclusive: waiting for running tests to finish'); said = true; } await sleep(700); }
    held.push(n);
  }
  return { slots: held, waitedMs: Date.now() - t0 };
}

export function releaseSlots(slots) { for (const n of slots) release(slotFile(n)); release(EXCL); }

/** The measurement lock. Returns { waitedMs, release() }. */
export async function measureLock(label, log) {
  const t0 = Date.now(); let said = false;
  while (!tryTake(MEASURE, { label })) { if (!said) { log(`waiting for the measurement lock (${label})`); said = true; } await sleep(150); }
  const got = Date.now();
  return { waitedMs: got - t0, acquiredAt: got, release: () => { release(MEASURE); return Date.now(); } };
}

export function releaseAllOwned() {
  for (let n = 1; n <= MAX_SLOTS; n++) release(slotFile(n));
  release(EXCL); release(MEASURE);
}

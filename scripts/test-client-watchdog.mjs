// The WebCodecs frame watchdog must never demote a session that is already painting.
//
// index.html arms the 5 s "no WebCodecs frame" watchdog from two handlers: ICE 'connected' /
// 'completed' and the peer connection's own 'connected'. The worker posts 'rendered' exactly
// once per session, and that is what clears the watchdog. Either handler can fire again after
// it — the second of the two state changes landing after the worker's message, ICE going from
// 'connected' to 'completed', a transient disconnect that recovers — and a watchdog armed then
// was never cleared: five seconds later `fallbackToVideo` sent a second offer and the session
// sat on the <video> element (path 3) for the rest of its life, with the picture having been
// fine all along. Seen through scripts/test-handler as "falling back to <video>: no WebCodecs
// frame within 5s" on a join whose stamps were already being painted.
//
// The functions are lifted out of the page by name and run against a fake clock, the same trick
// test-client-reconfig.mjs uses.
//
//   node scripts/test-client-watchdog.mjs [path/to/static]
import fs from 'node:fs';
import vm from 'node:vm';

const STATIC = process.argv.slice(2).filter((a) => a.charAt(0) !== '-')[0] || 'src-tauri/src/streamer/static';
const page = fs.readFileSync(STATIC + '/index.html', 'utf8');

let failures = 0;
const ok = (m) => console.log('ok  - ' + m);
const fail = (m) => { failures++; console.log('FAIL- ' + m); };

function takeFunction(name) {
  const at = page.search(new RegExp('(?:async\\s+)?function\\s+' + name + '\\s*\\('));
  if (at < 0) throw new Error('function ' + name + ' not found in index.html');
  const open = page.indexOf('{', at);
  let depth = 0;
  for (let i = open; i < page.length; i++) {
    if (page[i] === '{') depth++;
    else if (page[i] === '}' && --depth === 0) return page.slice(at, i + 1);
  }
  throw new Error('function ' + name + ' is unbalanced');
}

const strip = (src) => src.replace(/\/\*[\s\S]*?\*\//g, '').split('\n').map((l) => l.replace(/\/\/.*$/, '')).join('\n');

// --- the wiring, checked on the source -------------------------------------------------------
{
  const msg = strip(page);
  if (/m\.data\.type === 'rendered'\)\s*\{\s*webcodecsPainted = true;\s*onStreamLive\(\);/.test(msg)) {
    ok("the worker's 'rendered' message latches webcodecsPainted before onStreamLive");
  } else {
    fail("the worker's 'rendered' message does not latch webcodecsPainted");
  }
  if (/webcodecsPainted = false;/.test(strip(takeFunction('teardownSession')))) {
    ok('teardownSession clears the latch, so the next session is watched again');
  } else {
    fail('teardownSession does not clear webcodecsPainted');
  }
  const armers = (msg.match(/armTransformWatchdog\(\)/g) || []).length;
  if (armers >= 2) ok('the watchdog is still armed from ' + armers + ' call sites (the race this test exists for)');
}

// --- the behaviour, against a fake clock ------------------------------------------------------
function harness() {
  const calls = [];
  let clock = 0;
  const timers = [];
  const ctx = {
    calls,
    setTimeout(fn, ms) { const t = { fn, at: clock + ms, live: true }; timers.push(t); return t; },
    clearTimeout(t) { if (t) t.live = false; },
    setInterval() { return 0; },
    clearInterval() {},
    advance(ms) {
      clock += ms;
      for (const t of timers) if (t.live && t.at <= clock) { t.live = false; t.fn(); }
    },
    pending() { return timers.filter((t) => t.live).length; },
  };
  vm.createContext(ctx);
  const prelude = `
    let watchdog = null;
    let statsPoll = null;
    let webcodecsPainted = false;
    let transformStarted = true;
    let transformWatchdog = null;
    let icePending = false;
    let reconfiguring = false;
    let isLive = false;
    let joinHideTimer = 0;
    let streamInputArmed = false;
    function syncInputGate() {}
    const webcodecs = true;
    const useScriptTransform = true;
    const hasLegacyEncodedStreams = true;
    const FRAME_WATCHDOG_MS = 5000;
    const TRANSFORM_START_TIMEOUT_MS = 2500;
    let transformWatchdogArmed = false;
    const loadingEl = { classList: { remove() {} } };
    const joinEl = { classList: { add() {} }, style: {} };
    function fallbackToVideo(r) { calls.push('fallbackToVideo:' + r); }
    function retryWithLegacyTransform(r) { calls.push('retryWithLegacyTransform:' + r); }
    function clearReconfigWatchdog() {}
    function dropStaleCanvas() {}
    function startReconfigPoll() { calls.push('startReconfigPoll'); }
    function showError() {}
    function startLock() {}
    function noteSuppressed() {}
    function noteRefinedRefreshRate() {}
    function clampRefresh(r) { return r; }
    const REFRESH_REFINE_SAMPLES = 1;
    function getRefreshRate() { return { then() {} }; }
  `;
  const body = [takeFunction('armFrameWatchdog'), takeFunction('armTransformWatchdog'), takeFunction('onStreamLive')].join('\n');
  vm.runInContext(prelude + body + `
    this.api = {
      iceConnected: () => armTransformWatchdog(),
      pcConnected: () => armTransformWatchdog(),
      rendered: () => { webcodecsPainted = true; onStreamLive(); },
      teardown: () => { if (watchdog) { clearTimeout(watchdog); watchdog = null; } webcodecsPainted = false; },
      watchdog: () => watchdog,
    };
  `, ctx);
  return ctx;
}

function demotions(ctx) { return ctx.calls.filter((c) => /^(fallbackToVideo|retryWithLegacyTransform)/.test(c)); }

{
  // The case that shipped: ICE connected, first frame painted, then the peer connection's own
  // 'connected' (or ICE 'completed') arms the watchdog again.
  const h = harness();
  h.api.iceConnected();
  h.advance(300);
  h.api.rendered();
  h.advance(50);
  h.api.pcConnected();
  h.advance(10000);
  const d = demotions(h);
  if (d.length === 0 && !h.api.watchdog()) ok('a state change after the first painted frame does not re-arm the watchdog');
  else fail('a painting session was demoted after a late state change: ' + d.join(' | '));
}

{
  // Repeated state changes for the whole session (ICE flapping) never demote it either.
  const h = harness();
  h.api.iceConnected();
  h.api.rendered();
  for (let i = 0; i < 6; i++) { h.advance(2000); h.api.iceConnected(); h.api.pcConnected(); }
  h.advance(10000);
  if (demotions(h).length === 0) ok('ICE flapping after the first frame never demotes');
  else fail('ICE flapping demoted a painting session: ' + demotions(h).join(' | '));
}

{
  // The watchdog still does its job: no frame within 5 s of the wire coming up is a demotion.
  const h = harness();
  h.api.iceConnected();
  h.advance(5001);
  const d = demotions(h);
  if (d.length === 1 && /no WebCodecs frame within 5s/.test(d[0])) ok('no frame within 5 s still demotes (' + d[0].split(':')[0] + ')');
  else fail('a session with no frame was not demoted: ' + JSON.stringify(h.calls));
}

{
  // A new session after a teardown is watched again.
  const h = harness();
  h.api.iceConnected();
  h.api.rendered();
  h.api.teardown();
  h.api.iceConnected();
  h.advance(5001);
  if (demotions(h).length === 1) ok('the next session after a teardown is watched again');
  else fail('the latch survived a teardown: ' + JSON.stringify(h.calls));
}

if (failures) { console.log(failures + ' failure(s)'); process.exit(1); }
console.log('all passed');

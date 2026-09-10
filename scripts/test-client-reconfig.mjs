// Drives index.html's `/reconfig` epoch handling under Node with a fake fetch.
//
// Settings changes apply live through epoch bumps rather than a display teardown
// (`streamer/session.rs`: `bump_reconfig_epoch`, `bump_kick_epoch`), and the client polling
// loop is the whole other half of that mechanism. Nothing tested it, and its failure modes are
// all silent: a missed kick leaves a removed device streaming, a missed reconfig leaves a
// device on the old resolution forever, and a spurious one tears the session down on a poll.
//
// The page has no build step and its script is inline, so the functions are lifted out by name
// and run directly — the same trick `test-client-ice.mjs` uses, and the reason both exist
// rather than a browser test.
//
//   node scripts/test-client-reconfig.mjs [path/to/static]
import fs from 'node:fs';
import vm from 'node:vm';

const STATIC = process.argv[2] || 'src-tauri/src/streamer/static';
const page = fs.readFileSync(STATIC + '/index.html', 'utf8');

let failures = 0;
const ok = (m) => console.log('ok  - ' + m);
const fail = (m) => { failures++; console.log('FAIL- ' + m); };
const eq = (name, got, want) => (got === want ? ok(name + ' = ' + got) : fail(name + ' = ' + got + ', expected ' + want));

function takeConst(name) {
  const m = new RegExp('\\b(?:const|let)\\s+' + name + '\\s*=\\s*[^;]+;').exec(page);
  if (!m) throw new Error(name + ' not found in index.html');
  return m[0];
}

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

// The page's own state, plus the collaborators the lifted functions call into. Every one of
// those is recorded rather than stubbed away, because the assertions are about which of them
// runs and in what order.
const prelude = `
  let reconfigPoll = null;
  let reconfigEpoch = null;
  let kickEpoch = null;
  let reconfiguring = false;
  let kicked = false;
  let isLive = true;
  let degraded = 0;
  let refusedStreak = 0;
  let usingFallback = false;
  function markAlive() { calls.push('markAlive'); }
  function noteSuppressed() { calls.push('noteSuppressed'); }
  function evaluateLink() { calls.push('evaluateLink'); }
  function teardownSession() { calls.push('teardownSession'); }
  function stopLock() { calls.push('stopLock'); }
  function showLinkStatus(s) { calls.push('showLinkStatus:' + s); }
  function showError(m) { calls.push('showError:' + m); }
  function linkLost(m) { calls.push('linkLost:' + m); }
  function start(again) { calls.push('start:' + !!again); }
  function queueOutputPicker(id) { calls.push('queueOutputPicker:' + id); }
  function nowMs() { return clock; }
`;

const epilogue = `
  this.api = {
    startReconfigPoll, armProbe, probeTick, applyReconfig, kickDevice, reconfigure,
    clearReconfigWatchdog,
    state: () => ({ reconfigPoll, reconfigEpoch, kickEpoch, reconfiguring, kicked, isLive, refusedStreak, lastOfferedSinkId }),
    setLive: (v) => { isLive = v; },
    setDegraded: (v) => { degraded = v; },
    setReconfiguring: (v) => { reconfiguring = v; },
    grace: { PROBE_OK_MS, PROBE_FAST_MS, REFUSED_MS, RECONFIG_DEADLINE_MS },
  };
`;

const source = [
  prelude,
  takeConst('lastOfferedSinkId'),
  takeConst('PROBE_OK_MS'),
  takeConst('PROBE_FAST_MS'),
  takeConst('REFUSED_MS'),
  takeConst('RECONFIG_DEADLINE_MS'),
  'let reconfigWatchdog = null;',
  takeFunction('clearReconfigWatchdog'),
  takeFunction('startReconfigPoll'),
  takeFunction('armProbe'),
  takeFunction('probeTick'),
  takeFunction('applyReconfig'),
  takeFunction('kickDevice'),
  takeFunction('reconfigure'),
  epilogue,
].join('\n');

let clock = 0;
let response = null;   // what the next fetch resolves to, or a thrown error
let sinkResult = true; // what the stubbed SEAudio.setSpeaker resolves to
let timers = [];

// A fresh realm per case: the lifted block declares `let`s at script scope, so running it
// twice in one context is a redeclaration error, and the state under test is exactly what
// must not leak between cases.
function makePoller() {
  const calls = [];
  const localTimers = [];
  timers = localTimers;
  // `applyReconfig` tests `window.SEAudio` and then calls the bare global, which is the same
  // object in a browser and has to be made so here. What it does with the answer is the whole
  // of cases 9-11: `false` is the engine refusing, and a host that repeats the same request
  // must not be answered with a second picker.
  const seAudio = { setSpeaker: async (id) => { calls.push('setSpeaker:' + id); return sinkResult; } };
  const ctx = {
    console,
    calls,
    Promise,
    RegExp,
    SEAudio: seAudio,
    window: { SEAudio: seAudio },
    setTimeout: (fn, ms) => { localTimers.push({ fn, at: clock + ms }); return localTimers.length; },
    clearTimeout: (id) => { if (id) localTimers[id - 1] = null; },
    fetch: async () => (typeof response === 'function' ? response() : response),
  };
  ctx.self = ctx;
  ctx.clock = 0;
  Object.defineProperty(ctx, 'clock', { get: () => clock });
  vm.createContext(ctx);
  vm.runInContext(source, ctx, { filename: 'index.html:reconfig' });
  ctx.api.calls = calls;
  ctx.api.timers = localTimers;
  return ctx.api;
}

const api = makePoller();
const calls = api.calls;

const json = (body, status) => ({ ok: status === undefined || status === 200, status: status || 200, json: async () => body });
const settle = async () => { for (let i = 0; i < 6; i++) await new Promise((r) => setImmediate(r)); };
// Fire whichever timer the page armed, as if its deadline had arrived.
function runTimers() {
  const due = timers.filter(Boolean);
  timers.length = 0;
  due.forEach((t) => { clock = Math.max(clock, t.at); t.fn(); });
  return due.length;
}

// ── 1. the first poll adopts the host's epochs without reacting to them ──────────────────
// Adopting them is what makes the *next* difference meaningful. Treating the first read as a
// change would rebuild the session on every join.
calls.length = 0;
response = json({ epoch: 7, kick: 3 });
await api.startReconfigPoll();
eq('the first read adopts the reconfig epoch', api.state().reconfigEpoch, 7);
eq('and the kick epoch', api.state().kickEpoch, 3);
if (!calls.includes('teardownSession')) ok('adopting the epochs does not tear the session down');
else fail('the first /reconfig read rebuilt the session');
if (api.state().reconfigPoll) ok('the poll is armed after the first read');
else fail('no poll timer was armed');

// ── 2. an unchanged epoch is a no-op ─────────────────────────────────────────────────────
calls.length = 0;
api.applyReconfig({ epoch: 7, kick: 3 });
if (calls.length === 0) ok('an unchanged epoch does nothing at all');
else fail('an unchanged epoch ran ' + calls.join(', '));

// ── 3. a bumped reconfig epoch rebuilds, once ────────────────────────────────────────────
calls.length = 0;
api.applyReconfig({ epoch: 8, kick: 3 });
eq('the new epoch is adopted', api.state().reconfigEpoch, 8);
if (calls.includes('teardownSession') && calls.includes('start:true')) ok('a bumped epoch tears down and restarts the session');
else fail('a bumped epoch ran ' + calls.join(', '));
if (api.state().reconfiguring) ok('the page is marked reconfiguring so every recovery path stands down');
else fail('reconfiguring was not set');

calls.length = 0;
api.applyReconfig({ epoch: 9, kick: 3 });
if (!calls.includes('start:true')) ok('a second bump while already reconfiguring does not start a second session');
else fail('a concurrent bump started an overlapping session');

// ── 4. a rebuild that never completes is not left frozen ─────────────────────────────────
// While `reconfiguring` is set, the probe tick, the link evaluation, the connection-state
// handler and the leave beacon all return early — so without its own deadline a stuck rebuild
// leaves the last decoded frame on screen with no spinner and no reconnect.
calls.length = 0;
clock += api.grace.RECONFIG_DEADLINE_MS;
runTimers();
if (calls.some((c) => c.startsWith('linkLost'))) ok('a rebuild that misses its deadline surfaces as a lost link');
else fail('the reconfigure watchdog did not fire: ' + calls.join(', '));
if (!api.state().reconfiguring) ok('and the page stops standing down so recovery can run');
else fail('reconfiguring was still set after the watchdog');

// ── 5. a bumped kick epoch removes the device and stops everything ───────────────────────
calls.length = 0;
api.applyReconfig({ epoch: 9, kick: 4 });
if (calls.includes('teardownSession') && calls.some((c) => c.startsWith('showError'))) ok('a bumped kick epoch tears the session down and says why');
else fail('a kick ran ' + calls.join(', '));
eq('the device is marked kicked', api.state().kicked, true);
eq('and no longer live', api.state().isLive, false);
if (!calls.includes('start:true')) ok('a kick does not also try to reconnect');
else fail('a kick restarted the session');

calls.length = 0;
api.applyReconfig({ epoch: 20, kick: 99 });
if (calls.length === 0) ok('a kicked page ignores everything that follows');
else fail('a kicked page still ran ' + calls.join(', '));

// ── 6. a refused poll is counted, a 500 is not ───────────────────────────────────────────
// The distinction is what the link evaluation is built on: a connection refused fast is the
// host being gone, while a 500 is a host that is still there and answering.
{
  const p = makePoller();
  const calls = p.calls;
  response = () => { throw new Error('ECONNREFUSED'); };
  await p.probeTick();
  await settle();
  eq('a fast connection refusal counts against the link', p.state().refusedStreak, 1);
  if (calls.includes('evaluateLink')) ok('and the link is re-evaluated');
  else fail('a refused poll did not evaluate the link');

  response = json({}, 500);
  await p.probeTick();
  await settle();
  eq('a 500 from a host that is still answering resets the streak', p.state().refusedStreak, 0);
}

// ── 7. the poll backs off differently once the link is suspect ───────────────────────
// A separate poller per case: `armProbe` returns early once a timer is armed, which is what
// stops two polls from ever overlapping.
{
  const healthyPoller = makePoller();
  healthyPoller.armProbe();
  const healthy = healthyPoller.timers.filter(Boolean).pop();

  const suspectPoller = makePoller();
  suspectPoller.setDegraded(1);
  suspectPoller.armProbe();
  const suspect = suspectPoller.timers.filter(Boolean).pop();

  if (!healthy || !suspect) fail('armProbe did not arm a timer');
  else if (suspect.at < healthy.at) ok('a degraded link is polled faster than a healthy one');
  else fail('a degraded link polls no faster: ' + suspect.at + ' vs ' + healthy.at);

  const again = makePoller();
  again.armProbe();
  const first = again.timers.filter(Boolean).length;
  again.armProbe();
  if (again.timers.filter(Boolean).length === first) ok('an already-armed poll is not armed a second time');
  else fail('armProbe stacked a second timer on top of a live one');
}

// ── 8. a page that is not live arms nothing ──────────────────────────────────────────────
{
  const p = makePoller();
  const timers = p.timers;
  timers.length = 0;
  p.setLive(false);
  p.armProbe();
  if (timers.filter(Boolean).length === 0) ok('a page that is not live does not keep polling the host');
  else fail('a dead page armed a poll timer');

  p.setLive(true);
  p.setReconfiguring(true);
  timers.length = 0;
  p.armProbe();
  if (timers.filter(Boolean).length === 0) ok('a page mid-rebuild does not poll over its own reconfigure');
  else fail('a reconfiguring page armed a poll timer');
}

// ── 9. a sink the engine refuses is offered a picker once, not once a second ─────────────
// `process_reconfig` puts `audioSink` in *every* body, and an engine that cannot route to it
// answers `false` on every tick. Without a latch on the requested id the picker re-armed about
// once a second, and from then on every tap on the stage opened a browser chooser instead of
// re-entering fullscreen.
{
  const p = makePoller();
  const calls = p.calls;
  sinkResult = false;
  for (let i = 0; i < 10; i++) p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-A' });
  await settle();
  const offers = calls.filter((c) => c === 'queueOutputPicker:sink-A');
  eq('ten ticks of the same refused sink offer the picker once', offers.length, 1);
  eq('and every tick still tries to apply it', calls.filter((c) => c === 'setSpeaker:sink-A').length, 10);
  eq('the latch remembers which sink was offered', p.state().lastOfferedSinkId, 'sink-A');

  // A different sink is a different request and gets its own single offer.
  calls.length = 0;
  for (let i = 0; i < 5; i++) p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-B' });
  await settle();
  eq('a new sink the host asks for is offered once', calls.filter((c) => c === 'queueOutputPicker:sink-B').length, 1);
}

// ── 10. a sink that applies never offers a picker ────────────────────────────────────────
{
  const p = makePoller();
  const calls = p.calls;
  sinkResult = true;
  for (let i = 0; i < 5; i++) p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-C' });
  await settle();
  if (!calls.some((c) => c.startsWith('queueOutputPicker'))) ok('a sink that applies is never answered with a picker');
  else fail('a working sink change opened a picker: ' + calls.join(', '));
  eq('and the latch is untouched', p.state().lastOfferedSinkId, null);
}

// ── 11. `null` is "not yet", not a refusal ───────────────────────────────────────────────
// A sink commanded before the audio graph exists is remembered by audio.js and applied when it
// does. Reading that as a refusal would open a picker on a device that had not tried anything.
{
  const p = makePoller();
  const calls = p.calls;
  sinkResult = null;
  for (let i = 0; i < 5; i++) p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-D' });
  await settle();
  if (!calls.some((c) => c.startsWith('queueOutputPicker'))) ok('a sink that has not been tried yet is not answered with a picker');
  else fail('a deferred sink opened a picker: ' + calls.join(', '));
}
sinkResult = true;

if (failures) {
  console.log('\n' + failures + ' reconfig assertion(s) failed');
  process.exitCode = 1;
} else {
  console.log('\nall client reconfig assertions passed');
}

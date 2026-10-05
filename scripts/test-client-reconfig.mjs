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
  let currentJoinId = '';
  function setJoinId(id) { currentJoinId = id || ''; }
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
  function teardownSession(keepJoin) { calls.push('teardownSession:' + !!keepJoin); }
  function stopLock() { calls.push('stopLock'); }
  function showLinkStatus(s) { calls.push('showLinkStatus:' + s); }
  function showError(m) { calls.push('showError:' + m); }
  function linkLost(m) { calls.push('linkLost:' + m); }
  function start(again, keepJoin) { calls.push('start:' + !!again + ':' + !!keepJoin); }
  function queueOutputPicker(id) { calls.push('queueOutputPicker:' + id); }
  function nowMs() { return clock; }
`;

const epilogue = `
  this.api = {
    startReconfigPoll, armProbe, probeTick, applyReconfig, kickDevice, reconfigure,
    clearReconfigWatchdog, withJoin, releaseJoin,
    setJoinId: (v) => { currentJoinId = v || ''; },
    joinId: () => currentJoinId,
    state: () => ({ reconfigPoll, reconfigEpoch, kickEpoch, reconfiguring, kicked, isLive, refusedStreak, lastOfferedSinkId, appliedSinkId }),
    setLive: (v) => { isLive = v; },
    setDegraded: (v) => { degraded = v; },
    setReconfiguring: (v) => { reconfiguring = v; },
    grace: { PROBE_OK_MS, PROBE_FAST_MS, REFUSED_MS, RECONFIG_DEADLINE_MS },
  };
`;

const source = [
  prelude,
  takeConst('lastOfferedSinkId'),
  takeConst('appliedSinkId'),
  takeConst('PROBE_OK_MS'),
  takeConst('PROBE_FAST_MS'),
  takeConst('REFUSED_MS'),
  takeConst('RECONFIG_DEADLINE_MS'),
  'let reconfigWatchdog = null;',
  takeFunction('withJoin'),
  takeFunction('releaseJoin'),
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
  const urls = [];
  const beacons = [];
  const ctx = {
    console,
    calls,
    urls,
    beacons,
    Promise,
    RegExp,
    encodeURIComponent,
    navigator: { sendBeacon: (u) => { beacons.push(u); return true; } },
    sessionStorage: {
      _v: {},
      getItem(k) { return Object.prototype.hasOwnProperty.call(this._v, k) ? this._v[k] : null; },
      setItem(k, v) { this._v[k] = String(v); },
      removeItem(k) { delete this._v[k]; },
    },
    SEAudio: seAudio,
    window: { SEAudio: seAudio },
    setTimeout: (fn, ms) => { localTimers.push({ fn, at: clock + ms }); return localTimers.length; },
    clearTimeout: (id) => { if (id) localTimers[id - 1] = null; },
    fetch: async (url) => { urls.push(url); return typeof response === 'function' ? response() : response; },
  };
  ctx.self = ctx;
  ctx.clock = 0;
  Object.defineProperty(ctx, 'clock', { get: () => clock });
  vm.createContext(ctx);
  vm.runInContext(source, ctx, { filename: 'index.html:reconfig' });
  ctx.api.calls = calls;
  ctx.api.timers = localTimers;
  ctx.api.urls = urls;
  ctx.api.beacons = beacons;
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
if (!calls.some((c) => c.startsWith('start:'))) ok('adopting the epochs does not tear the session down');
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
// `start(true, true)` and not `start(true)`: the second argument is the whole fix for the
// owner's report. A reconfigure is the same session offering again, so the page presents the
// join id it already holds, the host keeps that session and its virtual display, and the
// dashboard never sees a leave followed by a join. Releasing it instead is what made a
// settings change read as a disconnect.
if (calls.includes('start:true:true')) ok('a bumped epoch renegotiates on the same join');
else fail('a bumped epoch ran ' + calls.join(', '));
if (!calls.includes('teardownSession:false')) ok('and never tears the join down on the way');
else fail('a reconfigure released its join: ' + calls.join(', '));
if (api.state().reconfiguring) ok('the page is marked reconfiguring so every recovery path stands down');
else fail('reconfiguring was not set');

calls.length = 0;
api.applyReconfig({ epoch: 9, kick: 3 });
if (!calls.some((c) => c.startsWith('start:'))) ok('a second bump while already reconfiguring does not start a second session');
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
if (calls.includes('teardownSession:false') && calls.some((c) => c.startsWith('showError'))) ok('a bumped kick epoch tears the session down and says why');
else fail('a kick ran ' + calls.join(', '));
eq('the device is marked kicked', api.state().kicked, true);
eq('and no longer live', api.state().isLive, false);
if (!calls.some((c) => c.startsWith('start:'))) ok('a kick does not also try to reconnect');
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
  // Ten polls a second apart, so each refusal has settled before the next one arrives — the
  // refusal clears the applied latch, so the request is retried rather than treated as done.
  for (let i = 0; i < 10; i++) { p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-A' }); await settle(); }
  const offers = calls.filter((c) => c === 'queueOutputPicker:sink-A');
  eq('ten ticks of the same refused sink offer the picker once', offers.length, 1);
  eq('and every tick still tries to apply it', calls.filter((c) => c === 'setSpeaker:sink-A').length, 10);
  eq('the latch remembers which sink was offered', p.state().lastOfferedSinkId, 'sink-A');

  // A different sink is a different request and gets its own single offer.
  calls.length = 0;
  for (let i = 0; i < 5; i++) { p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-B' }); await settle(); }
  eq('a new sink the host asks for is offered once', calls.filter((c) => c === 'queueOutputPicker:sink-B').length, 1);
}

// ── 10. a sink that applies never offers a picker ────────────────────────────────────────
{
  const p = makePoller();
  const calls = p.calls;
  sinkResult = true;
  for (let i = 0; i < 5; i++) { p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-C' }); await settle(); }
  if (!calls.some((c) => c.startsWith('queueOutputPicker'))) ok('a sink that applies is never answered with a picker');
  else fail('a working sink change opened a picker: ' + calls.join(', '));
  eq('and the latch is untouched', p.state().lastOfferedSinkId, null);
  // Class (c) is applied on change, not on every poll: `audioSink` is in *every* `/reconfig`
  // body, so without the diff this commanded the audio graph about once a second forever.
  eq('a sink that has not moved is commanded once, not once a poll', calls.filter((c) => c === 'setSpeaker:sink-C').length, 1);
  eq('and the applied latch names it', p.state().appliedSinkId, 'sink-C');

  // Class (c) never renegotiates: a sink change on its own leaves the epoch alone, so nothing
  // above it in `applyReconfig` fires.
  calls.length = 0;
  p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-D' });
  await settle();
  if (!calls.some((c) => c.startsWith('start:'))) ok('changing the output device renegotiates nothing');
  else fail('an audio-output change rebuilt the session: ' + calls.join(', '));
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

// ── 12. the join id names the session on every request ───────────────────────────────────
// The host will not resolve a request by the peer's address: an address moves under a live
// session and is handed to the next device that asks for one. Every session-scoped request
// therefore carries the id the host minted at `POST /whep`, and a tab that has none names
// nothing rather than something that happens to be nearby.
{
  const api = makePoller();
  api.setJoinId('0123456789abcdef0123456789abcdef');
  response = { ok: true, status: 200, json: async () => ({ epoch: 1, kick: 0 }) };
  await api.startReconfigPoll();
  eq('/reconfig names this tab\'s session', api.urls[0],
    '/reconfig?join=0123456789abcdef0123456789abcdef');

  api.urls.length = 0;
  clock += api.grace.PROBE_OK_MS;
  await api.probeTick();
  eq('and every later poll names it too', api.urls[0],
    '/reconfig?join=0123456789abcdef0123456789abcdef');
}
{
  const api = makePoller();
  response = { ok: true, status: 200, json: async () => ({ epoch: 1, kick: 0 }) };
  await api.startReconfigPoll();
  eq('a tab the host never named asks for nothing in particular', api.urls[0], '/reconfig');
}
// Releasing is the other half: a rejoin mints a new id, so the old session has to be given
// back or its display stands until the disconnect grace expires.
{
  const api = makePoller();
  api.setJoinId('ffffffffffffffffffffffffffffffff');
  api.releaseJoin();
  eq('leaving names the session being left', api.beacons[0],
    '/leave?join=ffffffffffffffffffffffffffffffff');
  eq('and the tab no longer claims it', api.joinId(), '');
  api.releaseJoin();
  eq('releasing twice sends one beacon', api.beacons.length, 1);
}
{
  const api = makePoller();
  api.setJoinId('11111111222222223333333344444444');
  api.releaseJoin('99999999888888887777777766666666');
  eq('an explicit id releases that one', api.beacons[0],
    '/leave?join=99999999888888887777777766666666');
  eq('and leaves this tab\'s own session alone', api.joinId(),
    '11111111222222223333333344444444');
}

// ── 12. a link that may come back keeps the session ─────────────────────────────────────
// The owner's report was "when display changes are made, the connected device disappears from
// my device list, then it reappears". A reconfigure that takes longer than the rebuild deadline
// ends in `linkLost`, and `linkLost` used to release the join — so the host was told the device
// had *left*, took the display down, raised `DeviceRemove`, and the reconnect that followed was
// a brand-new session with a brand-new display and every window shuffled back onto the primary
// monitor. Only a page that is going away or one the host has kicked releases a join; a lost
// link is what the disconnect grace exists for.
{
  const calls = [];
  const ctx = vm.createContext({ calls, console });
  vm.runInContext(
    [
      'let kicked = false; let usingFallback = true;',
      'function teardownSession(keepJoin) { calls.push("teardownSession:" + !!keepJoin); }',
      'function showLinkStatus(s) { calls.push("showLinkStatus"); }',
      'function showError(m) { calls.push("showError:" + m); }',
      'function start(again, keepJoin) { calls.push("start:" + !!again + ":" + !!keepJoin); }',
      'function offerHttpsUpgrade() { calls.push("offerHttpsUpgrade"); }',
      'function stopLock() { calls.push("stopLock"); }',
      'let isLive = true; let canWebCodecs = true;',
      'const location = { protocol: "https:" };',
      takeFunction('linkLost'),
      takeFunction('beginStreaming'),
      takeFunction('kickDevice'),
      'this.api = { linkLost, beginStreaming, kickDevice };',
    ].join('\n'),
    ctx
  );

  ctx.api.linkLost('Disconnected, reconnect to continue.');
  if (calls.includes('teardownSession:true')) ok('a lost link keeps the join the host is holding a display for');
  else fail('a lost link ran ' + calls.join(', '));
  if (!calls.includes('teardownSession:false')) ok('and never beacons a departure the page did not mean');
  else fail('a lost link released the join');

  calls.length = 0;
  ctx.api.beginStreaming();
  if (calls.includes('start:true:true')) ok('reconnecting presents the join this tab still holds');
  else fail('the reconnect ran ' + calls.join(', '));

  calls.length = 0;
  ctx.api.kickDevice();
  if (calls.includes('teardownSession:false')) ok('a kick still releases the join, because the host has already ended it');
  else fail('a kick ran ' + calls.join(', '));
}

// ── 13. one save, two epoch bumps (G04's ledger, item 2) ────────────────────────────────
// `set_device_override` and then `set_device_adaptive_modes` / `set_device_auto_frame_rate`
// each bump the epoch. The page stops polling while it rebuilds, so a bump that lands during
// the rebuild is only seen once the rebuilt stream is live and `onStreamLive` restarts the
// poll. That poll must act on it — anything else leaves the second change unapplied until
// some unrelated renegotiation.
{
  const p = makePoller();
  response = json({ epoch: 20, kick: 1 });
  await p.startReconfigPoll();
  p.calls.length = 0;
  // First bump seen: rebuild starts.
  p.applyReconfig({ epoch: 21, kick: 1 });
  if (p.calls.filter((c) => c === 'start:true:true').length === 1) ok('the first bump rebuilds');
  else fail('the first bump ran ' + p.calls.join(', '));
  eq('the rebuild is for epoch', p.state().reconfigEpoch, 21);
  // While it rebuilds the host bumps again, and nothing polls.
  response = json({ epoch: 22, kick: 1 });
  eq('a tick during the rebuild does nothing', (await p.probeTick(), p.calls.filter((c) => c === 'start:true:true').length), 1);
  // The rebuilt stream goes live: onStreamLive clears `reconfiguring` and restarts the poll.
  p.setReconfiguring(false);
  p.clearReconfigWatchdog();
  await p.startReconfigPoll();
  if (p.state().reconfigPoll) ok('the poll is re-armed after the rebuild');
  else fail('no poll after the rebuild');
  p.calls.length = 0;
  runTimers();
  await settle();
  if (p.calls.includes('start:true:true')) ok('the second bump, landed during the rebuild, rebuilds again on the next poll');
  else fail('the second bump was lost: ' + p.calls.join(', '));
  eq('and the page now holds the newer epoch', p.state().reconfigEpoch, 22);
}

if (failures) {
  console.log('\n' + failures + ' reconfig assertion(s) failed');
  process.exitCode = 1;
} else {
  console.log('\nall client reconfig assertions passed');
}

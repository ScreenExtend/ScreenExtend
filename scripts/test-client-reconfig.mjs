// Drives index.html's `/reconfig` epoch handling under Node with a fake fetch.
//
// Settings changes apply live through epoch bumps (`streamer/session.rs`: `bump_reconfig_epoch`,
// `bump_kick_epoch`), and the client polling loop is the other half of that mechanism. Its
// failure modes are all silent: a missed kick leaves a removed device streaming, a missed
// reconfig leaves a device on the old resolution, and a spurious one tears the session down.
//
// The page's script is inline with no build step, so the functions are lifted out by name.
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

// The page's own state, plus the collaborators the lifted functions call into, each recorded
// rather than stubbed away: the assertions are about which of them runs and in what order.
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
  // The real one is exercised end to end in test-client-join.mjs; here it records which way the
  // session ended and does the three things the assertions below read.
  function restorePreJoinPage(m, keepJoin) { calls.push('restorePreJoinPage:' + !!keepJoin); teardownSession(keepJoin); isLive = false; reconfiguring = false; showError(m); }
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
  takeFunction('sendGoodbye'),
  takeFunction('releaseJoin'),
  takeFunction('clearReconfigWatchdog'),
  takeFunction('armReconfigWatchdog'),
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
  // `applyReconfig` tests `window.SEAudio` and then calls the bare global: the same object in a
  // browser, so it has to be made so here. Cases 9-11 are about its answer: `false` is the
  // engine refusing.
  const seAudio = {
    setSpeaker: async (id) => { calls.push('setSpeaker:' + id); return sinkResult; },
    postOutputs: (force) => { calls.push('postOutputs:' + !!force); },
    noteOutputChoiceRequest: (id) => { calls.push('noteChoice:' + id); return 'queued'; },
    CHOOSE_OUTPUT: 'se:choose',
  };
  const urls = [];
  const beacons = [];
  const ctx = {
    console: { log: console.log, warn: (m) => calls.push('warn:' + m), error: console.error },
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
// fire whichever timer the page armed, as if its deadline had arrived
function runTimers() {
  const due = timers.filter(Boolean);
  timers.length = 0;
  due.forEach((t) => { clock = Math.max(clock, t.at); t.fn(); });
  return due.length;
}

// 1. The first poll adopts the host's epochs without reacting to them: treating the first
//    read as a change would rebuild the session on every join.
calls.length = 0;
response = json({ epoch: 7, kick: 3 });
await api.startReconfigPoll();
eq('the first read adopts the reconfig epoch', api.state().reconfigEpoch, 7);
eq('and the kick epoch', api.state().kickEpoch, 3);
if (!calls.some((c) => c.startsWith('start:'))) ok('adopting the epochs does not tear the session down');
else fail('the first /reconfig read rebuilt the session');
if (api.state().reconfigPoll) ok('the poll is armed after the first read');
else fail('no poll timer was armed');

// 2. An unchanged epoch is a no-op.
calls.length = 0;
api.applyReconfig({ epoch: 7, kick: 3 });
if (calls.length === 0) ok('an unchanged epoch does nothing at all');
else fail('an unchanged epoch ran ' + calls.join(', '));

// 3. A bumped reconfig epoch rebuilds, once.
calls.length = 0;
api.applyReconfig({ epoch: 8, kick: 3 });
eq('the new epoch is adopted', api.state().reconfigEpoch, 8);
// `start(true, true)`, not `start(true)`: a reconfigure is the same session offering again,
// so the page presents the join id it already holds, the host keeps that session and its
// virtual display, and the dashboard never sees a leave followed by a join.
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

// 4. A rebuild that never completes is not left frozen. While `reconfiguring` is set, the
//    probe tick, the link evaluation, the connection-state handler and the leave beacon all
//    return early, so without its own deadline a stuck rebuild leaves the last decoded frame
//    on screen with no spinner and no reconnect.
calls.length = 0;
clock += api.grace.RECONFIG_DEADLINE_MS;
runTimers();
if (calls.some((c) => c.startsWith('linkLost'))) ok('a rebuild that misses its deadline surfaces as a lost link');
else fail('the reconfigure watchdog did not fire: ' + calls.join(', '));
if (!api.state().reconfiguring) ok('and the page stops standing down so recovery can run');
else fail('reconfiguring was still set after the watchdog');

// 5. A bumped kick epoch removes the device and stops everything.
calls.length = 0;
api.applyReconfig({ epoch: 9, kick: 4 });
if (calls.includes('restorePreJoinPage:false')) ok('a kick ends the session through the one full restore, releasing the join');
else fail('a kick did not go through restorePreJoinPage: ' + calls.join(', '));
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

// 6. A refused poll is counted, a 500 is not: a connection refused fast is the host being
//    gone, while a 500 is a host that is still there and answering.
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

// 7. The poll backs off differently once the link is suspect. A separate poller per case:
//    `armProbe` returns early once a timer is armed, which is what stops two polls overlapping.
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

// 8. A page that is not live arms nothing.
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

// 9. A sink the engine refuses is reported once, and never offered over the stage.
//    `process_reconfig` puts `audioSink` in every body, and an engine that cannot route to it
//    answers `false` on every tick. A chooser raised for that would drop pointer lock, keyboard
//    lock and fullscreen for a request nobody at the device made, so a refusal is reported (a
//    log line and a forced `/audio-outputs` post carrying the sink in use), once per requested id.
{
  const p = makePoller();
  const calls = p.calls;
  sinkResult = false;
  // Each refusal has settled before the next poll arrives. A refusal clears the applied latch,
  // so the request is retried rather than treated as done.
  for (let i = 0; i < 10; i++) { p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-A' }); await settle(); }
  eq('a refused sink never arms the picker', calls.filter((c) => c.startsWith('queueOutputPicker')).length, 0);
  eq('ten ticks of the same refused sink report it once', calls.filter((c) => c.startsWith('warn:') && c.indexOf('sink-A') >= 0).length, 1);
  eq('and tell the host once what is actually playing', calls.filter((c) => c === 'postOutputs:true').length, 1);
  eq('and every tick still tries to apply it', calls.filter((c) => c === 'setSpeaker:sink-A').length, 10);
  eq('the latch remembers which sink was reported', p.state().lastOfferedSinkId, 'sink-A');

  // A different sink is a different request and gets its own single report.
  calls.length = 0;
  for (let i = 0; i < 5; i++) { p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-B' }); await settle(); }
  eq('a new refused sink is reported once', calls.filter((c) => c.startsWith('warn:') && c.indexOf('sink-B') >= 0).length, 1);
}

// 10. A sink that applies never offers a picker.
{
  const p = makePoller();
  const calls = p.calls;
  sinkResult = true;
  for (let i = 0; i < 5; i++) { p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-C' }); await settle(); }
  if (!calls.some((c) => c.startsWith('queueOutputPicker'))) ok('a sink that applies is never answered with a picker');
  else fail('a working sink change opened a picker: ' + calls.join(', '));
  eq('and the latch is untouched', p.state().lastOfferedSinkId, null);
  // applied on change, not on every poll: `audioSink` is in every `/reconfig` body, so without
  // the diff the audio graph is commanded about once a second
  eq('a sink that has not moved is commanded once, not once a poll', calls.filter((c) => c === 'setSpeaker:sink-C').length, 1);
  eq('and the applied latch names it', p.state().appliedSinkId, 'sink-C');

  // a sink change on its own leaves the epoch alone, so it never renegotiates
  calls.length = 0;
  p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-D' });
  await settle();
  if (!calls.some((c) => c.startsWith('start:'))) ok('changing the output device renegotiates nothing');
  else fail('an audio-output change rebuilt the session: ' + calls.join(', '));
}

// 11. `null` is "not yet", not a refusal: a sink commanded before the audio graph exists is
//     remembered by audio.js and applied when it does.
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

// 12. The join id names the session on every request. The host will not resolve a request by
//     the peer's address (an address moves under a live session and is handed to the next
//     device), so every session-scoped request carries the id the host minted at `POST /whep`,
//     and a tab that has none names nothing.
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

// 12. A link that may come back keeps the session. Only a page that is going away, or one the
//     host has kicked, releases a join. A lost link (including a reconfigure that misses the
//     rebuild deadline) keeps it: releasing tells the host the device left, so it takes the
//     display down, raises `DeviceRemove`, and the reconnect is a new session with a new display
//     and every window back on the primary monitor. The disconnect grace exists for this.
{
  const calls = [];
  const ctx = vm.createContext({ calls, console });
  vm.runInContext(
    [
      'let kicked = false; let usingFallback = true; let picturePending = false;',
      'function teardownSession(keepJoin) { calls.push("teardownSession:" + !!keepJoin); }',
      'function showLinkStatus(s) { calls.push("showLinkStatus"); }',
      'function showError(m) { calls.push("showError:" + m); }',
      'function start(again, keepJoin) { calls.push("start:" + !!again + ":" + !!keepJoin); }',
      'function offerHttpsUpgrade() { calls.push("offerHttpsUpgrade"); }',
      'function stopLock() { calls.push("stopLock"); }',
      'function restorePreJoinPage(m, keepJoin) { calls.push("restorePreJoinPage:" + !!keepJoin); teardownSession(keepJoin); isLive = false; showError(m); }',
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
  if (calls.includes('restorePreJoinPage:true')) ok('a lost link ends through the one full restore');
  else fail('a lost link did not go through restorePreJoinPage: ' + calls.join(', '));
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

// 13. One save, two epoch bumps. `set_device_override` and then `set_device_adaptive_modes` /
//     `set_device_auto_frame_rate` each bump the epoch. The page stops polling while it
//     rebuilds, so a bump that lands during the rebuild is only seen once the rebuilt stream is
//     live and `onStreamLive` restarts the poll. That poll must act on it, or the second change
//     stays unapplied until some unrelated renegotiation.
{
  const p = makePoller();
  response = json({ epoch: 20, kick: 1 });
  await p.startReconfigPoll();
  p.calls.length = 0;
  p.applyReconfig({ epoch: 21, kick: 1 });
  if (p.calls.filter((c) => c === 'start:true:true').length === 1) ok('the first bump rebuilds');
  else fail('the first bump ran ' + p.calls.join(', '));
  eq('the rebuild is for epoch', p.state().reconfigEpoch, 21);
  // while it rebuilds the host bumps again, and nothing polls
  response = json({ epoch: 22, kick: 1 });
  eq('a tick during the rebuild does nothing', (await p.probeTick(), p.calls.filter((c) => c === 'start:true:true').length), 1);
  // the rebuilt stream goes live: onStreamLive clears `reconfiguring` and restarts the poll
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

// 14. "Choose a speaker on this device..." raises nothing over the live stage. The request is
//     remembered and answered at the device's next join, before its stream (audio.js
//     `noteOutputChoiceRequest`). Every other value goes through too, since it is what re-arms
//     a later request.
{
  const p = makePoller();
  const calls = p.calls;
  sinkResult = true;
  for (let i = 0; i < 5; i++) { p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'se:choose' }); await settle(); }
  eq('the choose request is recorded once per change, not once a poll', calls.filter((c) => c === 'noteChoice:se:choose').length, 1);
  if (!/micCard|withModal|queueOutputPicker|showPermModal/.test(takeFunction('applyReconfig'))) ok('applyReconfig can raise no card, chooser or modal');
  else fail('applyReconfig still reaches a modal');
  if (!calls.some((c) => c.startsWith('queueOutputPicker') || c.startsWith('start:'))) ok('and it neither arms a picker nor renegotiates');
  else fail('the choose request did more than record itself: ' + calls.join(', '));
  calls.length = 0;
  p.applyReconfig({ epoch: 1, kick: 1, audioSink: 'sink-E' });
  await settle();
  eq('another sink is passed on so a later request is fresh', calls.filter((c) => c === 'noteChoice:sink-E').length, 1);
}

// The goodbye has to leave on browsers with no sendBeacon too: an iPhone whose tab is closed may
// never get another chance. Each fallback is probed, and the first that works is the only one used.
{
  const src = takeFunction('sendGoodbye');
  const run = (env) => {
    const sent = [];
    const ctx = Object.assign({ noteSuppressed() { sent.push('suppressed'); }, Date: { now: () => 7 } }, env(sent));
    vm.createContext(ctx);
    vm.runInContext(src + '\nthis.f = sendGoodbye;', ctx);
    return { path: ctx.f('/leave?join=ab'), sent };
  };
  const beacon = run((sent) => ({ navigator: { sendBeacon: (u) => { sent.push('beacon ' + u); return true; } } }));
  eq('a browser with sendBeacon uses it', beacon.path, 'beacon');
  eq('and sends nothing else', beacon.sent.join(','), 'beacon /leave?join=ab');
  function Req(u) { this.url = u; this.keepalive = false; }
  const fetchPath = run((sent) => ({
    navigator: {},
    Request: Req,
    fetch: (u, o) => { sent.push('fetch ' + u + ' ' + o.method + ' keepalive=' + o.keepalive); return Promise.resolve(); },
  }));
  eq('no sendBeacon: a keepalive fetch', fetchPath.path, 'fetch');
  eq('as a POST', fetchPath.sent[0], 'fetch /leave?join=ab POST keepalive=true');
  function Xhr() { this.open = (m, u, a) => { Xhr.log.push(m + ' ' + u + ' async=' + a); }; this.send = () => { Xhr.log.push('send'); }; }
  Xhr.log = [];
  const xhrPath = run(() => ({ navigator: {}, fetch: () => Promise.resolve(), Request: function (u) { this.url = u; }, XMLHttpRequest: Xhr }));
  eq('no keepalive in fetch: a synchronous XHR', xhrPath.path, 'xhr');
  eq('a blocking POST, so unload waits for it', Xhr.log.join(','), 'POST /leave?join=ab async=false,send');
  const images = [];
  function Img() { Object.defineProperty(this, 'src', { set(v) { images.push(v); } }); }
  const imagePath = run(() => ({
    navigator: { sendBeacon: () => false },
    XMLHttpRequest: function () { this.open = () => { throw new Error('blocked'); }; },
    Image: Img,
  }));
  eq('only an image request left: it is used', imagePath.path, 'image');
  eq('a GET with a cache-buster', images[0], '/leave?join=ab&t=7');
  const none = run(() => ({ navigator: {} }));
  eq('nothing at all: it says so and throws nothing', none.path, '');
}

if (failures) {
  console.log('\n' + failures + ' reconfig assertion(s) failed');
  process.exitCode = 1;
} else {
  console.log('\nall client reconfig assertions passed');
}

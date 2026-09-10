// Drives the ICE gathering wait out of index.html under Node with a fake RTCPeerConnection.
// The page has no build step and its script is inline, so the functions are lifted out of the
// file by name and run directly — the same trick would break on a bundler and is exactly why
// this file exists rather than a browser test.
//
// What it protects: a relayed (TURN) join must not end gathering on a host candidate. The
// offer is a single WHEP POST with no PATCH to trickle candidates later, so a relay candidate
// that has not been gathered by then can never be used, and the session simply fails to
// connect with nothing on screen to say why.
import fs from 'node:fs';
import vm from 'node:vm';

const args = process.argv.slice(2);
const UPDATE = args.indexOf('--update') >= 0;
const STATIC = args.filter((a) => a.charAt(0) !== '-')[0] || 'src-tauri/src/streamer/static';
const page = fs.readFileSync(STATIC + '/index.html', 'utf8');
// The golden `application/trickle-ice-sdpfrag` body, emitted here and parsed by
// `the_client_fragment_shape_parses_whole` in webrtc_session.rs. Regenerate with --update.
const FIXTURE = STATIC.replace(/[\\/]static\/?$/, '') + '/test/trickle-fragment.sdpfrag';

let failures = 0;
function ok(msg) { console.log('ok  - ' + msg); }
function fail(msg) { failures++; console.log('FAIL- ' + msg); }

// --- lift the pieces under test out of the page --------------------------------------------

function takeConst(name) {
  const m = new RegExp('\\bconst\\s+' + name + '\\s*=\\s*[^;]+;').exec(page);
  if (!m) throw new Error('const ' + name + ' not found in index.html');
  return m[0];
}

function takeFunction(name) {
  const at = page.indexOf('function ' + name + '(');
  if (at < 0) throw new Error('function ' + name + ' not found in index.html');
  const open = page.indexOf('{', at);
  let depth = 0;
  for (let i = open; i < page.length; i++) {
    const ch = page[i];
    if (ch === '{') depth++;
    else if (ch === '}') {
      depth--;
      if (depth === 0) return page.slice(at, i + 1);
    }
  }
  throw new Error('function ' + name + ' is unbalanced');
}

function takeLet(name) {
  const m = new RegExp('\\blet\\s+' + name + '\\s*=\\s*[^;]+;').exec(page);
  if (!m) throw new Error('let ' + name + ' not found in index.html');
  return m[0];
}

// The stage hold's real deadline is 8 s, which is not a thing to sit through in a unit suite.
// Its *value* is asserted from the page below; only the lifted copy is shortened, and the
// rewrite has to match or this throws rather than silently testing nothing.
const HOLD_DEADLINE_MS = 120;
function takeShortenedDeadline() {
  const src = takeConst('ICE_SETTLE_DEADLINE_MS');
  const shortened = src.replace(/=\s*\d+\s*;/, '= ' + HOLD_DEADLINE_MS + ';');
  if (shortened === src) throw new Error('could not shorten ICE_SETTLE_DEADLINE_MS: ' + src);
  return shortened;
}

const source = [
  takeConst('ICE_HOST_GRACE_MS'),
  takeConst('ICE_RELAY_GRACE_MS'),
  takeConst('TRICKLE_MAX_BATCH'),
  takeConst('TRICKLE_FIRST_ACK_MS'),
  takeShortenedDeadline(),
  takeLet('icePending'),
  takeLet('liveHeld'),
  takeLet('iceSettleTimer'),
  takeFunction('trickleIsFatal'),
  takeFunction('iceCandidateType'),
  takeFunction('iceGraceFor'),
  takeFunction('waitForIceGathering'),
  takeFunction('hasTurnServer'),
  takeFunction('relayNeeded'),
  takeFunction('localIceUfrag'),
  takeFunction('trickleFragment'),
  takeFunction('startTrickle'),
  takeFunction('holdStageForIce'),
  takeFunction('armIceSettleDeadline'),
  takeFunction('releaseIceHold'),
  takeFunction('iceSettled'),
  // `const` is lexically scoped to the script and never reaches the context object.
  'this.__grace = { host: ICE_HOST_GRACE_MS, relay: ICE_RELAY_GRACE_MS };',
  'this.__maxBatch = TRICKLE_MAX_BATCH;',
  'this.__firstAckMs = TRICKLE_FIRST_ACK_MS;',
  'this.holdStageForIce = holdStageForIce;',
  'this.armIceSettleDeadline = armIceSettleDeadline;',
  'this.releaseIceHold = releaseIceHold;',
  'this.iceSettled = iceSettled;',
  'this.__holdPending = function () { return icePending; };',
].join('\n');

let deadlineBits = 0;
let suppressed = 0;
let liveShown = 0;
const patches = [];
let nextPatchStatus = 204;
const ctx = {
  console,
  setTimeout,
  clearTimeout,
  Promise,
  RegExp,
  DEG: { ICE_GATHERING_DEADLINE: 1 << 15, TRICKLE_ICE_UNAVAILABLE: 1 << 22 },
  degrade(bit) { deadlineBits |= bit; },
  noteSuppressed() { suppressed++; },
  onStreamLive() { liveShown++; },
  fetch(url, opts) {
    patches.push({ url, opts });
    // A request that is accepted by the socket and then never answered — the case the
    // first-ack deadline exists for. A promise that never settles is exactly that.
    if (ctx.__hangFetch) return new Promise(() => {});
    const status = nextPatchStatus;
    return Promise.resolve({ status, ok: status >= 200 && status < 300 });
  },
};
vm.createContext(ctx);
vm.runInContext(source, ctx, { filename: 'index.html:ice' });

// --- who actually needs a relay ------------------------------------------------------------
// The host serves TURN to same-subnet clients on purpose, as a backup, so the presence of a TURN
// entry says nothing about whether the session will be relayed. Only the host's own locality
// does, and it is the field `/ice-config` was extended to carry.

const TURN = [{ urls: ['turn:relay.example:3478'], username: 'u', credential: 'c' }];
const STUN = [{ urls: 'stun:stun.example:19302' }];
const relayCases = [
  [{ locality: 'same-network', iceServers: TURN }, false, 'a LAN session does not wait for a relay, even with TURN configured'],
  [{ locality: 'same-network', iceServers: [] }, false, 'a LAN session with no servers does not wait for a relay'],
  [{ locality: 'cross-network', iceServers: TURN }, true, 'a cross-network session with TURN waits for a relay'],
  [{ locality: 'cross-network', iceServers: STUN }, false, 'a cross-network session with no TURN cannot wait for a relay it will never get'],
  [{ locality: 'unknown', iceServers: TURN }, true, 'an older host with no locality field falls back to the server list'],
  [{ locality: 'unknown', iceServers: [] }, false, 'an older host with no TURN takes the LAN branch'],
];
for (const [cfg, want, msg] of relayCases) {
  const got = ctx.relayNeeded(cfg);
  if (got !== want) fail(msg + ' (got ' + got + ')');
  else ok(msg);
}

// --- the pure gating table -----------------------------------------------------------------

const HOST = ctx.__grace.host;
const RELAY = ctx.__grace.relay;
if (!(HOST > 0) || !(RELAY > HOST)) fail('grace constants read back as ' + HOST + '/' + RELAY);
else ok('the grace constants are ' + HOST + ' ms host, ' + RELAY + ' ms relay');

const table = [
  ['host', false, HOST, 'a host candidate ends a LAN wait'],
  ['srflx', false, HOST, 'a reflexive candidate ends a LAN wait'],
  ['relay', false, 0, 'a relay candidate does not shorten a LAN wait'],
  ['host', true, 0, 'a host candidate does NOT end a relayed wait'],
  ['srflx', true, 0, 'a reflexive candidate does NOT end a relayed wait'],
  ['relay', true, RELAY, 'a relay candidate ends a relayed wait'],
  ['prflx', true, 0, 'an unknown type never ends a relayed wait'],
];
for (const [type, needRelay, want, why] of table) {
  const got = ctx.iceGraceFor(type, needRelay);
  if (got !== want) fail(why + ' (grace ' + got + ', expected ' + want + ')');
  else ok(why);
}

if (ctx.iceCandidateType({ type: 'relay' }) !== 'relay') fail('the type field is preferred');
else ok('the candidate type field is used when present');
if (ctx.iceCandidateType({ candidate: 'candidate:1 1 udp 41885439 10.0.0.1 5000 typ relay raddr' }) !== 'relay') {
  fail('the SDP candidate line is parsed when there is no type field');
} else ok('the candidate type is parsed out of the SDP line as a fallback');
if (ctx.iceCandidateType({ candidate: '' }) !== '') fail('an empty candidate is not a type');
else ok('an empty candidate line yields no type');

// --- the wait itself, against a fake peer connection ---------------------------------------

function fakePc() {
  const subs = {};
  return {
    iceGatheringState: 'gathering',
    addEventListener(t, fn) { (subs[t] = subs[t] || []).push(fn); },
    removeEventListener(t, fn) { subs[t] = (subs[t] || []).filter((f) => f !== fn); },
    emitCandidate(type) {
      const c = { type, candidate: 'candidate:1 1 udp 1 10.0.0.1 1 typ ' + type, sdpMid: '0', sdpMLineIndex: 0 };
      this.emit({ candidate: c });
    },
    emit(ev) {
      (subs.icecandidate || []).slice().forEach((fn) => fn(ev));
    },
    complete() {
      this.iceGatheringState = 'complete';
      (subs.icegatheringstatechange || []).slice().forEach((fn) => fn());
    },
    listenerCount() { return Object.keys(subs).reduce((n, k) => n + subs[k].length, 0); },
  };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function timed(fn) {
  const t0 = Date.now();
  await fn();
  return Date.now() - t0;
}

// --- trickle ICE, RFC 9725 §4.6 --------------------------------------------------------------
// The half that replaces the wait above: the offer leaves immediately and the candidates follow
// over PATCH. What has to hold is that nothing is sent before the host names a resource, that a
// candidate the offer already carried is not sent twice, and that the body is the sdpfrag the
// host's parser reads.

function settle() { return new Promise((r) => setTimeout(r, 0)); }

function cand(text, mid) {
  return { candidate: text, sdpMid: mid === undefined ? '0' : mid, sdpMLineIndex: 0, usernameFragment: 'UF01' };
}

async function trickle() {
  // The fragment body.
  {
    const body = ctx.trickleFragment('ABCD', [cand('candidate:1 1 udp 2 10.0.0.1 5000 typ host')], false);
    const lines = body.split('\r\n').filter((l) => l.length);
    const want = [
      'a=ice-ufrag:ABCD',
      'm=video 9 UDP/TLS/RTP/SAVPF 0',
      'a=mid:0',
      'a=candidate:1 1 udp 2 10.0.0.1 5000 typ host',
    ];
    if (lines.join('|') !== want.join('|')) fail('fragment body is ' + JSON.stringify(lines));
    else ok('a fragment is ufrag, one m= section, the mid and the candidate');
  }
  {
    // The host tolerates a bare `candidate:` prefix, but the attribute name must still be there.
    const body = ctx.trickleFragment('', [cand('1 1 udp 2 10.0.0.1 5000 typ host')], true);
    if (body.indexOf('a=candidate:1 1 udp') < 0) fail('a candidate with no prefix was not normalised');
    else ok('a candidate line with no "candidate:" prefix gets one');
    if (body.indexOf('a=ice-ufrag') >= 0) fail('an empty ufrag was written anyway');
    else ok('an unknown ufrag is simply omitted');
    if (body.indexOf('a=end-of-candidates') < 0) fail('end-of-candidates was not written');
    else ok('the last fragment carries a=end-of-candidates');
  }
  {
    const body = ctx.trickleFragment('U', [], true);
    if (body.indexOf('m=') >= 0) fail('an empty batch still wrote an m= section');
    else ok('a bare end-of-candidates fragment has no m= section');
  }

  // localIceUfrag reads the offer, for an engine with no usernameFragment on the candidate.
  if (ctx.localIceUfrag({ localDescription: { sdp: 'v=0\r\na=ice-ufrag:ZZZZ\r\n' } }) !== 'ZZZZ') {
    fail('the ufrag was not read out of the local description');
  } else ok('the ufrag falls back to the one in the local offer');
  if (ctx.localIceUfrag({ localDescription: null }) !== '') fail('a missing local description was not tolerated');
  else ok('no local description yields no ufrag');

  // Nothing may go out before the answer names a resource.
  {
    patches.length = 0;
    const pc = fakePc();
    const t = ctx.startTrickle(pc);
    pc.emitCandidate('host');
    await settle();
    if (patches.length) fail('a candidate was PATCHed before the host named a resource');
    else ok('candidates are buffered until the answer carries a Location');
    t.open('/whep/abc', 'v=0\r\n');
    await settle();
    if (patches.length !== 1) fail('the buffered candidate was not flushed on open (' + patches.length + ' PATCHes)');
    else if (patches[0].url !== '/whep/abc') fail('PATCH went to ' + patches[0].url);
    else if (patches[0].opts.method !== 'PATCH') fail('the method was ' + patches[0].opts.method);
    else if (patches[0].opts.headers['Content-Type'] !== 'application/trickle-ice-sdpfrag') {
      fail('content type was ' + patches[0].opts.headers['Content-Type']);
    } else ok('opening the resource flushes the buffer as one PATCH of trickle-ice-sdpfrag');
    if (t.count() !== 1) fail('the sent count is ' + t.count());
    else ok('a 204 counts the candidate as sent');
    t.stop();
    if (pc.listenerCount() !== 0) fail('stop() left ' + pc.listenerCount() + ' listeners behind');
    else ok('stop() removes the candidate listener');
  }

  // A candidate the offer already carried must not be sent again.
  {
    patches.length = 0;
    const pc = fakePc();
    const t = ctx.startTrickle(pc);
    const inOffer = 'candidate:1 1 udp 2 10.0.0.1 5000 typ host';
    pc.emit({ candidate: cand(inOffer) });
    pc.emit({ candidate: cand('candidate:2 1 udp 2 10.0.0.2 5001 typ srflx') });
    t.open('/whep/abc', 'v=0\r\na=' + inOffer + '\r\n');
    await settle();
    if (patches.length !== 1) fail('expected one PATCH, got ' + patches.length);
    else if (patches[0].opts.body.indexOf('10.0.0.1') >= 0) fail('a candidate already in the offer was trickled again');
    else if (patches[0].opts.body.indexOf('10.0.0.2') < 0) fail('the new candidate was not trickled');
    else ok('a candidate the offer already carried is not sent a second time');
    t.stop();
  }

  // A null candidate is the end of gathering, and it closes the sender.
  {
    patches.length = 0;
    const pc = fakePc();
    const t = ctx.startTrickle(pc);
    t.open('/whep/abc', '');
    pc.emit({ candidate: null });
    await settle();
    if (patches.length !== 1) fail('the end of gathering sent ' + patches.length + ' PATCHes');
    else if (patches[0].opts.body.indexOf('a=end-of-candidates') < 0) fail('the last PATCH did not say end-of-candidates');
    else ok('a null candidate PATCHes a=end-of-candidates');
    if (pc.listenerCount() !== 0) fail('the sender stayed subscribed after end-of-candidates');
    else ok('the sender unsubscribes once gathering is over');
  }

  // A 404 is the session being over: stop, do not retry.
  {
    patches.length = 0;
    nextPatchStatus = 404;
    const pc = fakePc();
    const t = ctx.startTrickle(pc);
    t.open('/whep/gone', '');
    pc.emitCandidate('host');
    await settle();
    await settle();
    const after = patches.length;
    pc.emitCandidate('srflx');
    await settle();
    if (patches.length !== after) fail('a candidate was PATCHed after a 404');
    else ok('a 404 stops the sender rather than retrying into a dead session');
    if (t.count() !== 0) fail('a 404 was counted as a sent candidate');
    else ok('a 404 counts nothing as sent');
    nextPatchStatus = 204;
    t.stop();
  }

  // A transient failure retries once; a second failure is the endpoint, and because the offer
  // that went out carries no candidates at all, that has to reach the page rather than be a
  // silent drop.
  {
    patches.length = 0;
    nextPatchStatus = 500;
    let deadReason = null;
    const pc = fakePc();
    const t = ctx.startTrickle(pc, (r) => { deadReason = r; });
    t.open('/whep/abc', '');
    pc.emitCandidate('relay');
    for (let i = 0; i < 6; i++) await settle();
    if (patches.length !== 2) fail('a failed PATCH was retried ' + (patches.length - 1) + ' times, expected exactly 1');
    else if (patches[1].opts.body.indexOf('typ relay') < 0) fail('the retry did not carry the candidate back');
    else ok('a failed PATCH puts its candidates back and retries once');
    if (!deadReason) fail('a second failure did not report the sender dead');
    else ok('a second failure in a row reports the sender dead: ' + deadReason);
    const after = patches.length;
    pc.emitCandidate('host');
    for (let i = 0; i < 4; i++) await settle();
    if (patches.length > after) fail('the sender kept PATCHing after reporting dead (' + patches.length + ')');
    else ok('a dead sender stops rather than spinning against the endpoint');
    if (pc.listenerCount() !== 0) fail('a dead sender stayed subscribed');
    else ok('a dead sender unsubscribes');
    nextPatchStatus = 204;
    t.stop();
  }

  // A status that means "this verb will never work here" does not wait for a second try.
  for (const status of [403, 405, 415, 501]) {
    if (!ctx.trickleIsFatal(status)) { fail(status + ' is not treated as fatal'); continue; }
    patches.length = 0;
    nextPatchStatus = status;
    let deadReason = null;
    const pc = fakePc();
    const t = ctx.startTrickle(pc, (r) => { deadReason = r; });
    t.open('/whep/abc', '');
    pc.emitCandidate('host');
    for (let i = 0; i < 4; i++) await settle();
    if (patches.length !== 1) fail(status + ' was retried instead of failing at once');
    else if (!deadReason) fail(status + ' did not report the sender dead');
    else ok('PATCH ' + status + ' fails at once rather than retrying a verb the endpoint refuses');
    nextPatchStatus = 204;
    t.stop();
  }
  if (ctx.trickleIsFatal(404) || ctx.trickleIsFatal(500) || ctx.trickleIsFatal(204)) {
    fail('a non-fatal status was classed as fatal');
  } else ok('404, 500 and 204 are not fatal statuses');

  // A PATCH that is never answered at all: nothing above can fire, so the deadline is the only
  // thing that notices.
  {
    patches.length = 0;
    let deadReason = null;
    const pc = fakePc();
    const t = ctx.startTrickle(pc, (r) => { deadReason = r; });
    ctx.__hangFetch = true;
    t.open('/whep/abc', '');
    pc.emitCandidate('host');
    await sleep(ctx.__firstAckMs + 200);
    if (!deadReason) fail('a black-holed PATCH was never noticed');
    else ok('a PATCH that never settles is reported dead after the first-ack deadline');
    ctx.__hangFetch = false;
    t.stop();
  }

  // Nothing was ever attempted — a network with no candidates at all — so there is nothing to
  // blame on the endpoint and no rejoin to ask for.
  {
    let deadReason = null;
    const pc = fakePc();
    const t = ctx.startTrickle(pc, (r) => { deadReason = r; });
    t.open('/whep/abc', '');
    await sleep(ctx.__firstAckMs + 200);
    if (deadReason) fail('a session that produced no candidates was blamed on the endpoint');
    else ok('no candidates means no PATCH and no failure to report');
    t.stop();
  }

  // ── the settle callback ────────────────────────────────────────────────────────────────
  // The page holds the streaming stage back until the sender says it will send nothing more,
  // so that a rejoin with a gathered offer can only ever happen in front of the join form.
  // Every exit has to reach it, or the picture never appears.

  // The happy path: the last batch carries `a=end-of-candidates` and is acknowledged.
  {
    patches.length = 0;
    let settles = 0;
    const pc = fakePc();
    const t = ctx.startTrickle(pc, null, () => { settles++; });
    t.open('/whep/abc', '');
    pc.emitCandidate('host');
    await settle();
    if (settles) fail('the sender settled while candidates were still coming');
    else ok('a sender with candidates in flight has not settled');
    pc.emit({ candidate: null });
    for (let i = 0; i < 6; i++) await settle();
    const last = patches[patches.length - 1];
    if (!last || last.opts.body.indexOf('a=end-of-candidates') < 0) fail('no end-of-candidates was sent');
    else if (settles !== 1) fail('the acknowledged end-of-candidates settled ' + settles + ' times');
    else ok('an acknowledged end-of-candidates settles the sender exactly once');
  }

  // A dead sender settles too — after `onDead`, so a rejoin replaces the gate rather than
  // racing it, and the page is never left holding a stage for a sender that gave up.
  {
    patches.length = 0;
    nextPatchStatus = 405;
    const order = [];
    const pc = fakePc();
    const t = ctx.startTrickle(pc, () => order.push('dead'), () => order.push('settle'));
    t.open('/whep/abc', '');
    pc.emitCandidate('host');
    for (let i = 0; i < 6; i++) await settle();
    if (order.join(',') !== 'dead,settle') fail('a dead sender ran ' + order.join(',') + ', expected dead,settle');
    else ok('a dead sender reports the failure first and settles after it');
    nextPatchStatus = 204;
    t.stop();
    if (order.filter((x) => x === 'settle').length !== 1) fail('stop() settled a dead sender a second time');
    else ok('settling is once-only however many exits are taken');
  }

  // The session is over: 404 is not a transport failure, but it is still the end of sending.
  {
    patches.length = 0;
    nextPatchStatus = 404;
    let settles = 0;
    let deadReason = null;
    const pc = fakePc();
    const t = ctx.startTrickle(pc, (r) => { deadReason = r; }, () => { settles++; });
    t.open('/whep/abc', '');
    pc.emitCandidate('host');
    for (let i = 0; i < 6; i++) await settle();
    if (deadReason) fail('a 404 was reported as a transport failure: ' + deadReason);
    else if (settles !== 1) fail('a 404 settled ' + settles + ' times');
    else ok('a 404 settles the sender without blaming the endpoint');
    nextPatchStatus = 204;
  }

  // ── the end of gathering has two signals, and either one has to be enough ──────────────
  // Waiting only for the null candidate makes the whole sender — and with it the streaming
  // stage — depend on one event. A browser that reports completion through the state alone,
  // or one whose null candidate landed in the task before the listener was attached, then
  // never sends `a=end-of-candidates`, never settles, and the page reports a gathering
  // timeout on a session that gathered in a hundred milliseconds.
  {
    patches.length = 0;
    let settles = 0;
    const pc = fakePc();
    const t = ctx.startTrickle(pc, null, () => { settles++; });
    t.open('/whep/abc', '');
    pc.emitCandidate('host');
    await settle();
    pc.complete();
    for (let i = 0; i < 6; i++) await settle();
    const last = patches[patches.length - 1];
    if (!last || last.opts.body.indexOf('a=end-of-candidates') < 0) {
      fail('gathering completing without a null candidate sent no end-of-candidates');
    } else if (settles !== 1) {
      fail('gathering completing without a null candidate settled ' + settles + ' times');
    } else ok('iceGatheringState "complete" ends the sender even with no null candidate');
    if (pc.listenerCount() !== 0) fail('the sender left ' + pc.listenerCount() + ' listeners behind');
    else ok('both of the sender listeners are removed on the way out');
    t.stop();
  }

  // Gathering that was already over before the sender existed fires no further event: the
  // sender is built one task after `setLocalDescription` resolves, so this is a real ordering,
  // not a hypothetical one.
  {
    patches.length = 0;
    let settles = 0;
    const pc = fakePc();
    pc.iceGatheringState = 'complete';
    const t = ctx.startTrickle(pc, null, () => { settles++; });
    t.open('/whep/abc', '');
    for (let i = 0; i < 6; i++) await settle();
    if (!patches.length || patches[0].opts.body.indexOf('a=end-of-candidates') < 0) {
      fail('a sender built after gathering finished sent no end-of-candidates');
    } else if (settles !== 1) fail('it settled ' + settles + ' times');
    else ok('a sender built after gathering already completed still settles');
    t.stop();
  }

  // Every candidate was already in the offer, so the batch is empty. Without an end-of-
  // gathering signal that is a sender which PATCHes nothing, reports nothing and never
  // settles — and `open()`'s first-ack timer deliberately stays quiet, because nothing was
  // attempted. The stage would then wait out the whole deadline for a healthy session.
  {
    patches.length = 0;
    let settles = 0;
    let deadReason = null;
    const inOffer = 'candidate:1 1 udp 2 10.0.0.1 5000 typ host';
    const pc = fakePc();
    const t = ctx.startTrickle(pc, (r) => { deadReason = r; }, () => { settles++; });
    pc.emit({ candidate: cand(inOffer) });
    t.open('/whep/abc', 'v=0\r\na=' + inOffer + '\r\n');
    await settle();
    if (patches.length) fail('a duplicate-only batch was PATCHed anyway');
    else ok('a batch of nothing but duplicates sends no PATCH');
    pc.complete();
    for (let i = 0; i < 6; i++) await settle();
    if (settles !== 1) fail('a duplicate-only sender settled ' + settles + ' times');
    else if (deadReason) fail('it was blamed on the endpoint: ' + deadReason);
    else ok('a sender whose only candidates were already in the offer still settles');
    t.stop();
  }

  // A network that produces no candidate at all: no PATCH, nothing to blame, and still an end.
  {
    patches.length = 0;
    let settles = 0;
    const pc = fakePc();
    const t = ctx.startTrickle(pc, null, () => { settles++; });
    t.open('/whep/abc', '');
    pc.complete();
    for (let i = 0; i < 6; i++) await settle();
    if (settles !== 1) fail('a session with no candidates settled ' + settles + ' times');
    else ok('a session that gathered no candidates at all still settles');
    t.stop();
  }

  // An older call site passes no callback at all; the sender must not throw on its way out.
  {
    const pc = fakePc();
    const t = ctx.startTrickle(pc);
    t.open('/whep/abc', '');
    pc.emit({ candidate: null });
    for (let i = 0; i < 4; i++) await settle();
    ok('a sender with no settle callback finishes without throwing');
    t.stop();
  }

  // The golden vector the Rust parser is tested against. It is produced by the *shipped*
  // `trickleFragment`, not by a copy of it, so a change to the client body either updates this
  // file — and the Rust test then parses the new bytes — or fails here. A hand-written literal on
  // the Rust side could not do either.
  {
    const body = ctx.trickleFragment('UF01', [
      cand('candidate:1 1 udp 2130706431 192.168.1.5 54321 typ host'),
      cand('candidate:2 1 udp 41885439 203.0.113.7 40000 typ relay raddr 0.0.0.0 rport 0'),
    ], true);
    const want = fs.existsSync(FIXTURE) ? fs.readFileSync(FIXTURE, 'utf8') : null;
    if (UPDATE) {
      fs.writeFileSync(FIXTURE, body);
      ok('wrote the golden fragment to ' + FIXTURE);
    } else if (want === null) {
      fail(FIXTURE + ' is missing; re-run with --update');
    } else if (want !== body) {
      fail('the client fragment no longer matches ' + FIXTURE + '; re-run with --update so the '
        + 'Rust parser test moves with it');
    } else {
      ok('the shipped fragment still matches the golden vector the Rust parser is tested on');
    }
  }

  // A duplicate sitting behind a fresh candidate is still filtered.
  {
    patches.length = 0;
    const pc = fakePc();
    const t = ctx.startTrickle(pc);
    const inOffer = 'candidate:9 1 udp 2 10.9.9.9 5000 typ host';
    pc.emit({ candidate: cand('candidate:8 1 udp 2 10.8.8.8 5000 typ srflx') });
    pc.emit({ candidate: cand(inOffer) });
    t.open('/whep/abc', 'v=0\r\na=' + inOffer + '\r\n');
    await settle();
    if (patches.length !== 1) fail('expected one PATCH, got ' + patches.length);
    else if (patches[0].opts.body.indexOf('10.9.9.9') >= 0) fail('a duplicate behind a fresh candidate was trickled');
    else if (patches[0].opts.body.indexOf('10.8.8.8') < 0) fail('the fresh candidate was dropped by the filter');
    else ok('the duplicate filter covers the whole batch, not just its head');
    t.stop();
  }

  // More candidates than one batch holds go out in order, one request at a time.
  {
    patches.length = 0;
    const pc = fakePc();
    const t = ctx.startTrickle(pc);
    const n = ctx.__maxBatch + 3;
    for (let i = 0; i < n; i++) pc.emit({ candidate: cand('candidate:' + i + ' 1 udp 2 10.1.0.' + i + ' 5000 typ host') });
    t.open('/whep/abc', '');
    for (let i = 0; i < 6; i++) await settle();
    if (patches.length !== 2) fail(n + ' candidates went out in ' + patches.length + ' PATCHes, expected 2');
    else if (t.count() !== n) fail('the sent count is ' + t.count() + ', expected ' + n);
    else ok('a burst larger than one batch is split and every candidate still goes out');
    t.stop();
  }
}

// --- the streaming stage's hold ------------------------------------------------------------
// The hold keeps the picture back until the trickle sender says it will send nothing more, so
// that a rejoin with a gathered offer can only ever happen in front of the join form. Its
// deadline is the net under that. What this section pins is *when the net is armed*: at the
// offer it also covered the WHEP round trip, and that round trip builds a virtual display —
// `DISPLAY_ATTACH_TIMEOUT` is 5 s on its own and the correlation lock serialises it across
// joins — so a host that was merely slow made the page report `ice_gathering_timed_out` for a
// join that then succeeded, and hold the picture for 8 s to do it.

async function stageHold() {
  const shipped = /=\s*(\d+)\s*;/.exec(takeConst('ICE_SETTLE_DEADLINE_MS'));
  const whep = /=\s*(\d+)\s*;/.exec(takeConst('WHEP_TIMEOUT_MS'));
  if (!shipped || Number(shipped[1]) !== 8000) fail('the shipped settle deadline is ' + (shipped && shipped[1]));
  else ok('the shipped settle deadline is 8000 ms (tested here at ' + HOLD_DEADLINE_MS + ')');
  if (!whep) fail('WHEP_TIMEOUT_MS is not a plain number any more');
  else ok('the WHEP POST has its own ' + whep[1] + ' ms timeout, which is what bounds a host that never answers');

  // Armed at the offer, this is where the false timeout came from: the sender cannot PATCH
  // before the answer names its resource, so every millisecond the host spends creating a
  // display counted against a deadline about ICE.
  {
    deadlineBits = 0;
    liveShown = 0;
    ctx.holdStageForIce();
    await sleep(HOLD_DEADLINE_MS * 3);
    if (deadlineBits & ctx.DEG.ICE_GATHERING_DEADLINE) fail('the hold expired while the WHEP POST was still in flight');
    else ok('holding the stage does not start the deadline — nothing can settle before the answer');
    if (!ctx.__holdPending()) fail('the hold released itself with no settle');
    else ok('the hold is still in force while the answer is outstanding');
  }

  // Armed when the resource is opened, it is a real deadline again.
  {
    deadlineBits = 0;
    liveShown = 0;
    ctx.armIceSettleDeadline();
    await sleep(HOLD_DEADLINE_MS + 120);
    if (!(deadlineBits & ctx.DEG.ICE_GATHERING_DEADLINE)) fail('the armed deadline never fired');
    else ok('once the resource is open the deadline runs, and reports bit 15');
    if (ctx.__holdPending()) fail('the expired deadline left the stage held');
    else ok('the expired deadline releases the stage rather than holding it for ever');
  }

  // A settle inside the deadline cancels it, and shows a picture that was waiting on it.
  {
    deadlineBits = 0;
    liveShown = 0;
    ctx.holdStageForIce();
    ctx.armIceSettleDeadline();
    ctx.iceSettled();
    await sleep(HOLD_DEADLINE_MS * 3);
    if (deadlineBits & ctx.DEG.ICE_GATHERING_DEADLINE) fail('a settled hold still reported a timeout');
    else ok('a settle inside the deadline cancels it and reports nothing');
  }

  // Re-arming is idempotent: `armIceSettleDeadline` is called once per join, but a second call
  // must not restart the clock and give a wedged sender another eight seconds.
  {
    deadlineBits = 0;
    ctx.holdStageForIce();
    ctx.armIceSettleDeadline();
    await sleep(HOLD_DEADLINE_MS * 0.6);
    ctx.armIceSettleDeadline();
    await sleep(HOLD_DEADLINE_MS * 0.6 + 80);
    if (!(deadlineBits & ctx.DEG.ICE_GATHERING_DEADLINE)) fail('a second arm restarted the deadline');
    else ok('arming twice does not restart the deadline');
    ctx.releaseIceHold();
  }
}

// --- what the offer waits for, in milliseconds --------------------------------------------
// The one number this change is about: how long `setLocalDescription` resolving is from the
// WHEP POST going out. Trickle removes the wait entirely, so the measured quantity is the wait
// itself. Three network shapes, five trials each, median reported — the timers are `setTimeout`
// so the floor is the event loop, not the code.

async function measure() {
  const TRIALS = 5;
  const median = (xs) => xs.slice().sort((a, b) => a - b)[Math.floor(xs.length / 2)];

  async function trial(fn) {
    const xs = [];
    for (let i = 0; i < TRIALS; i++) xs.push(await timed(fn));
    return median(xs);
  }

  // A LAN join: a host candidate lands in the first millisecond and the wait ends one host
  // grace later.
  const lan = await trial(async () => {
    const pc = fakePc();
    const p = ctx.waitForIceGathering(pc, 3000, false);
    pc.emitCandidate('host');
    await p;
  });

  // A relayed join with a reachable TURN server: the allocation is a round trip behind the host
  // candidate, and the wait cannot end on anything before it.
  const relayed = await trial(async () => {
    const pc = fakePc();
    const p = ctx.waitForIceGathering(pc, 6000, true);
    pc.emitCandidate('host');
    await sleep(300);
    pc.emitCandidate('relay');
    await p;
  });

  // A relayed join whose TURN server never answers: libwebrtc retransmits the ALLOCATE on the
  // RFC 5389 schedule and the wait runs to its deadline. Shortened to 600 ms here so the harness
  // does not sit for six seconds; the shipped deadline is 6000.
  const dead = await trial(async () => {
    const pc = fakePc();
    await ctx.waitForIceGathering(pc, 600, true);
  });

  // With trickle there is no wait: the offer is posted from `setLocalDescription` and every
  // candidate follows over PATCH.
  console.log('\n--- what the offer waits for (median of ' + TRIALS + ') ---');
  console.log('  gather-then-post, LAN                 ' + lan + ' ms');
  console.log('  gather-then-post, relayed (TURN 300ms)' + String(relayed).padStart(5) + ' ms');
  console.log('  gather-then-post, TURN unreachable    ' + dead + ' ms (of a 600 ms deadline; shipped is 6000)');
  console.log('  trickle                               0 ms');
  if (lan < ctx.__grace.host) fail('the LAN wait measured below its own grace constant');
  if (relayed <= lan) fail('a relayed wait measured no longer than a LAN one');
}

async function run() {
  {
    const pc = fakePc();
    const took = await timed(async () => {
      const p = ctx.waitForIceGathering(pc, 3000, false);
      await sleep(10);
      pc.emitCandidate('host');
      await p;
    });
    if (took < HOST || took > HOST + 400) fail('LAN wait took ' + took + ' ms, expected about ' + HOST);
    else ok('a LAN wait ends one host grace after the first host candidate');
    if (pc.listenerCount() !== 0) fail('the LAN wait left ' + pc.listenerCount() + ' listeners behind');
    else ok('the wait removes both of its listeners');
  }

  {
    const pc = fakePc();
    let settled = false;
    const p = ctx.waitForIceGathering(pc, 3000, true).then(() => { settled = true; });
    await sleep(10);
    pc.emitCandidate('host');
    pc.emitCandidate('srflx');
    await sleep(HOST + 250);
    if (settled) fail('a relayed wait ended on host/srflx candidates');
    else ok('a relayed wait is still open after host and reflexive candidates');
    const t0 = Date.now();
    pc.emitCandidate('relay');
    await p;
    const took = Date.now() - t0;
    if (took < RELAY || took > RELAY + 400) fail('relay grace was ' + took + ' ms, expected about ' + RELAY);
    else ok('a relayed wait ends one relay grace after the relay candidate');
  }

  {
    const pc = fakePc();
    const took = await timed(async () => {
      const p = ctx.waitForIceGathering(pc, 3000, true);
      await sleep(10);
      pc.complete();
      await p;
    });
    if (took > 300) fail('gathering-complete took ' + took + ' ms to end the wait');
    else ok('gathering completing ends a relayed wait immediately');
  }

  {
    deadlineBits = 0;
    const pc = fakePc();
    const took = await timed(async () => {
      await ctx.waitForIceGathering(pc, 300, true);
    });
    if (took < 300 || took > 900) fail('the deadline fired at ' + took + ' ms, expected 300');
    else ok('a relayed wait with no relay candidate runs to its deadline');
    if (!(deadlineBits & ctx.DEG.ICE_GATHERING_DEADLINE)) fail('reaching the deadline set no degradation bit');
    else ok('reaching the deadline sets ICE_GATHERING_DEADLINE');
  }

  {
    const pc = fakePc();
    pc.iceGatheringState = 'complete';
    const took = await timed(() => ctx.waitForIceGathering(pc, 6000, true));
    if (took > 100) fail('an already-complete gathering state still waited ' + took + ' ms');
    else ok('an already-complete gathering state resolves without waiting');
  }

  await trickle();
  await stageHold();
  await measure();

  if (failures) {
    console.log('\n' + failures + ' ICE assertion(s) failed');
    process.exitCode = 1;
  } else {
    console.log('\nall client ICE assertions passed');
  }
}

run();

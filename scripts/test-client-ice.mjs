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

const STATIC = process.argv[2] || 'src-tauri/src/streamer/static';
const page = fs.readFileSync(STATIC + '/index.html', 'utf8');

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

const source = [
  takeConst('ICE_HOST_GRACE_MS'),
  takeConst('ICE_RELAY_GRACE_MS'),
  takeFunction('iceCandidateType'),
  takeFunction('iceGraceFor'),
  takeFunction('waitForIceGathering'),
  takeFunction('hasTurnServer'),
  takeFunction('relayNeeded'),
  // `const` is lexically scoped to the script and never reaches the context object.
  'this.__grace = { host: ICE_HOST_GRACE_MS, relay: ICE_RELAY_GRACE_MS };',
].join('\n');

let deadlineBits = 0;
const ctx = {
  console,
  setTimeout,
  clearTimeout,
  Promise,
  RegExp,
  DEG: { ICE_GATHERING_DEADLINE: 1 << 15 },
  degrade(bit) { deadlineBits |= bit; },
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
      const c = { type, candidate: 'candidate:1 1 udp 1 10.0.0.1 1 typ ' + type };
      (subs.icecandidate || []).slice().forEach((fn) => fn({ candidate: c }));
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

  if (failures) {
    console.log('\n' + failures + ' ICE assertion(s) failed');
    process.exitCode = 1;
  } else {
    console.log('\nall client ICE assertions passed');
  }
}

run();

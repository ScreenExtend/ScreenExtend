// The join path of index.html: what a failed setup leads to, and the load-time warm-up that keeps
// two first-use costs off the path between Connect and the offer.
//
// A lost WHEP answer used to demote the session to <video> for good (`fallbackToVideo` on
// "The host did not answer in time"), although nothing about the video path had failed. And on a
// browser's first join the offer left 1.9-2.3 s after the click, nearly all of it the first
// `enumerateDevices()` and the first `new RTCPeerConnection()` (docs/overhaul-new/G07.md).
//
// Functions are lifted out of the page by name, as test-client-reconfig.mjs does.
//
//   node scripts/test-client-join.mjs [path/to/static]
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

// ---- setupFailureAction
{
  const ctx = vm.createContext({});
  vm.runInContext(takeConst('ANSWER_TIMEOUT_TEXT') + '\n' + takeFunction('setupFailureAction') + '\nthis.f = setupFailureAction; this.T = ANSWER_TIMEOUT_TEXT;', ctx);
  const f = ctx.f;
  const timeout = new Error(ctx.T);
  const base = { answerRetried: false, webcodecsTier: true, reconfiguringLive: false };
  eq('lost answer on the WebCodecs tier', f(timeout, base), 'retry');
  eq('lost answer on <video>', f(timeout, { ...base, webcodecsTier: false }), 'retry');
  eq('lost answer during a live reconfigure', f(timeout, { ...base, reconfiguringLive: true }), 'retry');
  eq('second lost answer on the WebCodecs tier', f(timeout, { ...base, answerRetried: true }), 'video');
  eq('second lost answer on <video>, first join', f(timeout, { ...base, answerRetried: true, webcodecsTier: false }), 'error');
  eq('bad OTP never demotes', f(new Error('Invalid session ID or OTP.'), base), 'error');
  eq('bad OTP during a live reconfigure keeps the picture', f(new Error('Invalid session ID or OTP.'), { ...base, reconfiguringLive: true }), 'poll');
  eq('host refused the POST (5xx)', f(new Error('WHEP POST failed: 500 join failed'), base), 'retry');
  eq('connection dropped', f(new TypeError('Failed to fetch'), base), 'retry');
  eq('client error from the host still demotes', f(new Error('WHEP POST failed: 400 bad sdp'), base), 'video');
  eq('decoder-side setup error still demotes', f(new Error('createEncodedStreams failed'), base), 'video');
  eq('other setup error on <video> mid-reconfigure', f(new Error('x'), { ...base, webcodecsTier: false, reconfiguringLive: true }), 'poll');
  eq('a missing error object', f(null, { ...base, webcodecsTier: false }), 'error');

  // The timeout is raised with exactly that text, or the classification never sees it.
  if (/throw new Error\(ANSWER_TIMEOUT_TEXT\)/.test(page)) ok('the WHEP deadline throws ANSWER_TIMEOUT_TEXT');
  else fail('the WHEP deadline no longer throws ANSWER_TIMEOUT_TEXT');
  // Only an *accepted* answer may refill the budget: reset before the 5xx throw, a host that
  // keeps refusing would be offered to forever.
  const startSrc = takeFunction('start');
  const resetAt = startSrc.indexOf('answerRetried = false;');
  const refusedAt = startSrc.indexOf('if (!resp.ok) throw');
  if (resetAt > refusedAt && refusedAt > 0) ok('the retry budget is refilled only after a 2xx answer');
  else fail('the retry budget is refilled before the refused-POST check');
  if (/answerRetried = false;[\s\S]{0,40}/.test(startSrc) && /answerRetried = false;/.test(page.slice(page.indexOf("addEventListener('submit'")))) ok('the retry budget is reset by an answer and by Connect');
  else fail('the retry budget is not reset by an answer and by Connect');
}

// ---- warmJoinPath
function runWarm(env) {
  const calls = [];
  const ctx = vm.createContext({ calls, noteSuppressed: () => calls.push('suppressed'), navigator: env.navigator, RTCPeerConnection: env.RTCPeerConnection });
  vm.runInContext(takeFunction('warmJoinPath') + '\nwarmJoinPath();', ctx);
  return calls;
}
{
  const calls = [];
  const navigator = {
    mediaDevices: {
      enumerateDevices() { calls.push('enumerate'); return Promise.resolve([]); },
      getUserMedia() { calls.push('getUserMedia'); return Promise.resolve(null); },
    },
    permissions: { query() { calls.push('permissions'); return Promise.resolve({ state: 'prompt' }); } },
  };
  function RTCPeerConnection() { calls.push('pc'); }
  RTCPeerConnection.prototype.close = function () { calls.push('close'); };
  runWarm({ navigator, RTCPeerConnection });
  eq('warm-up enumerates once', calls.filter((c) => c === 'enumerate').length, 1);
  eq('warm-up builds and closes one peer connection', calls.filter((c) => c === 'pc').length + '/' + calls.filter((c) => c === 'close').length, '1/1');
  eq('warm-up asks for nothing that can prompt', calls.filter((c) => c === 'getUserMedia' || c === 'permissions').length, 0);

  const r1 = runWarm({ navigator: {}, RTCPeerConnection: undefined });
  eq('warm-up on an engine with neither API', r1.length, 0);
  const r2 = runWarm({
    navigator: { mediaDevices: { enumerateDevices() { throw new Error('nope'); } } },
    RTCPeerConnection: function () { throw new Error('nope'); },
  });
  eq('warm-up swallows and reports throwing APIs', r2.filter((c) => c === 'suppressed').length, 2);
  let rejected = null;
  const r3calls = runWarm({
    navigator: { mediaDevices: { enumerateDevices() { rejected = Promise.reject(new Error('denied')); return rejected; } } },
    RTCPeerConnection: undefined,
  });
  await Promise.resolve(rejected).catch(() => {});
  await new Promise((r) => setTimeout(r, 0));
  eq('a rejected enumeration is reported, not thrown', r3calls.filter((c) => c === 'suppressed').length, 1);

  // Scheduled at load, after the task that parses the page, and never awaited by the join.
  if (/setTimeout\(warmJoinPath,\s*0\)/.test(page)) ok('warmJoinPath is scheduled at load');
  else fail('warmJoinPath is not scheduled at load');
  const submit = page.slice(page.indexOf("addEventListener('submit'"));
  if (!/warmJoinPath/.test(submit)) ok('the join does not wait for the warm-up');
  else fail('the submit handler references warmJoinPath');
}

// ---- sendLeave: a page going away releases its join whatever state it is in
function runLeave({ live, reconfiguring, joinId }) {
  const beacons = [];
  const calls = [];
  const ctx = vm.createContext({
    beacons, calls, encodeURIComponent,
    navigator: { sendBeacon: (u) => { beacons.push(u); return true; } },
    noteSuppressed: () => calls.push('suppressed'),
  });
  vm.runInContext(`
    let currentJoinId = ${JSON.stringify(joinId)};
    function setJoinId(id) { currentJoinId = id || ''; }
    let isLive = ${live}; let reconfiguring = ${reconfiguring};
    let pc = { close() { calls.push('pcclose'); } };
    let leaveSent = false;
    ${takeFunction('releaseJoin')}
    ${takeFunction('sendLeave')}
    sendLeave(); sendLeave();
    this.after = currentJoinId;
  `, ctx);
  return { beacons, calls, after: ctx.after };
}
{
  const id = 'a'.repeat(32);
  for (const [what, st] of [['live', { live: true, reconfiguring: false }], ['mid-reconfigure', { live: true, reconfiguring: true }], ['answered, no frame yet', { live: false, reconfiguring: false }]]) {
    const r = runLeave({ ...st, joinId: id });
    eq('pagehide ' + what + ': one /leave beacon', r.beacons.length + ':' + (r.beacons[0] || ''), '1:/leave?join=' + id);
    eq('pagehide ' + what + ': the peer connection is closed once', r.calls.filter((c) => c === 'pcclose').length, 1);
  }
  const none = runLeave({ live: false, reconfiguring: false, joinId: '' });
  eq('pagehide with no join id sends nothing', none.beacons.length, 0);
}

if (failures) { console.log(failures + ' failure(s)'); process.exit(1); }
console.log('all client join assertions passed');

// The join path of index.html: what a failed setup leads to, and the load-time warm-up that keeps
// two first-use costs off the path between Connect and the offer. Unwarmed, a browser's first
// offer left 1.9-2.3 s after the click, nearly all of it the first `enumerateDevices()` and the
// first `new RTCPeerConnection()`.
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

{
  const ctx = vm.createContext({});
  vm.runInContext(takeConst('ANSWER_TIMEOUT_TEXT') + '\n' + takeFunction('setupFailureAction') + '\nthis.f = setupFailureAction; this.T = ANSWER_TIMEOUT_TEXT;', ctx);
  const f = ctx.f;
  const timeout = new Error(ctx.T);
  const base = { answerRetried: false, webcodecsTier: true };
  const refused = new Error('WHEP POST failed: 500 join failed: virtual display 0 did not attach within timeout');
  eq('lost answer on the WebCodecs tier', f(timeout, base), 'retry');
  eq('lost answer on <video>', f(timeout, { ...base, webcodecsTier: false }), 'retry');
  // A host-side failure is retried once and then ends the session. It never demotes to <video>:
  // that would be a third offer (and a third display attempt) on a host already failing.
  eq('second lost answer on the WebCodecs tier ends, no demotion', f(timeout, { ...base, answerRetried: true }), 'error');
  eq('second lost answer on <video>', f(timeout, { ...base, answerRetried: true, webcodecsTier: false }), 'error');
  eq('host refused the POST (5xx)', f(refused, base), 'retry');
  eq('host refused the retry too: error, not <video>', f(refused, { ...base, answerRetried: true }), 'error');
  eq('bad OTP never demotes', f(new Error('Invalid session ID or OTP.'), base), 'error');
  eq('connection dropped', f(new TypeError('Failed to fetch'), base), 'retry');
  eq('connection dropped twice', f(new TypeError('Failed to fetch'), { ...base, answerRetried: true }), 'error');
  eq('client error from the host still demotes', f(new Error('WHEP POST failed: 400 bad sdp'), base), 'video');
  eq('decoder-side setup error still demotes', f(new Error('createEncodedStreams failed'), base), 'video');
  eq('other setup error on <video>', f(new Error('x'), { ...base, webcodecsTier: false }), 'error');
  eq('a missing error object', f(null, { ...base, webcodecsTier: false }), 'error');
  if (!/'poll'/.test(takeFunction('setupFailureAction')) && !/action === 'poll'/.test(takeFunction('start'))) ok("no 'poll' outcome: a failed renegotiation cannot leave a frozen frame up");
  else fail("a failed setup can still end in 'poll' (frozen frame, locks held)");

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

// The join settles every prompt before the offer: permission work (output detection, the
// microphone card, the browser's output chooser) completes before the stream starts, so nothing
// is raised over a live stage and the host builds no display for someone still reading a card.
// The card has no timeout of its own (Skip ends it), so no join-path deadline may run while it
// is up.
{
  const submit = page.slice(page.indexOf("addEventListener('submit'"));
  const at = (s) => submit.indexOf(s);
  const prep = at('await SEAudio.prepareOutputsForJoin(');
  if (prep > 0) ok('the submit handler awaits the output preparation');
  else fail('the submit handler does not await prepareOutputsForJoin');
  for (const [what, s] of [['the offer (beginStreaming)', 'beginStreaming();'], ['the audio-prepare deadline', 'AUDIO_PREPARE_DEADLINE_MS'], ['the refresh-rate deadline', 'resolveRefreshRate()'], ['the loading state', "loadingEl.classList.add('show')"]]) {
    if (prep > 0 && at(s) > prep) ok('the card wait comes before ' + what);
    else fail(what + ' starts before the card wait');
  }
  const firstAwait = at('await ');
  // the chooser is started on every Connect, inside the click, and its promise is handed to
  // the preparation rather than started there
  const picker = at('SEAudio.startPicker()');
  if (picker > 0 && picker < firstAwait) ok("the browser's chooser is started before the first await (inside the click's activation)");
  else fail('the chooser is started after an await, or not at all');
  if (/prepareOutputsForJoin\(micCard, pickerDone\)/.test(submit)) ok('and the preparation awaits that same chooser');
  else fail('the preparation does not receive the chooser started in the click');
  if (at('connectBtn.disabled = true;') > 0 && at('connectBtn.disabled = true;') < prep) ok('Connect is disabled before the wait, so the join cannot be submitted twice');
  else fail('Connect stays enabled while the card is up');
  const reconfig = takeFunction('applyReconfig');
  if (!/micCard|showPermModal|withModal|queueOutputPicker|pickOutput/.test(reconfig)) ok('nothing in the /reconfig handler can raise a card or a chooser');
  else fail('applyReconfig can still raise UI over a live stage');
  eq('the card is referenced only by its definition and the join', (page.match(/\bmicCard\b/g) || []).length, 2);
  if (/prepareOutputsForJoin\(micCard,/.test(submit)) ok('and the join is the one that passes it');
  else fail('the join does not hand the card to the output preparation');
}

// The whole page: what a join arms, and that every way it ends gives all of it back. A live
// stream arms input.js's capture-phase keydown (which `preventDefault()`s, so no Ctrl+C and no
// typing), the wake lock, fullscreen, pointer/keyboard lock, the leave prompt, pollers,
// watchdogs and the audio session, and a failed join must pass through `teardownSession` or
// input.js stays armed for the life of the page.
//
// The real input.js and the page's inline script run in one realm against fake DOM, WebRTC,
// fetch and timer surfaces. SEAudio is a recording stub (audio.js's own `endSession` is covered
// in test-client-audio.mjs), and input.js's data channels never open.
const SCRIPT = (() => {
  const re = /<script>([\s\S]*?)<\/script>/g;
  let m;
  while ((m = re.exec(page))) if (m[1].indexOf("getElementById('joinForm')") >= 0) return m[1];
  throw new Error('inline page script not found');
})();
const INPUT_JS = fs.readFileSync(STATIC + '/input.js', 'utf8');
const realFlush = async () => { for (let i = 0; i < 40; i++) await new Promise((r) => setImmediate(r)); };

function bootPage(o) {
  o = o || {};
  const log = { fetches: [], beacons: [], pcs: [], workers: [], sentinels: [], wakeRequests: 0, audio: [], errors: [] };
  let clock = 0;
  let seq = 0;
  const timers = [];
  const addTimer = (fn, ms, every, args) => {
    const t = { id: ++seq, fn: () => fn.apply(null, args), at: clock + Math.max(0, +ms || 0), every, live: true };
    timers.push(t);
    return t.id;
  };
  const kill = (id) => { const t = timers.find((x) => x.id === id); if (t) t.live = false; };

  const winListeners = {};
  const docListeners = {};
  const listen = (reg) => ({
    add(t, f) { (reg[t] = reg[t] || []).push(f); },
    remove(t, f) { const a = reg[t]; if (!a) return; const i = a.indexOf(f); if (i >= 0) a.splice(i, 1); },
  });
  const W = listen(winListeners);
  const D = listen(docListeners);

  const doc = {
    fullscreenElement: null,
    pointerLockElement: null,
    visibilityState: 'visible',
    body: null,
    addEventListener: (t, f) => D.add(t, f),
    removeEventListener: (t, f) => D.remove(t, f),
    exitFullscreen() { doc.fullscreenElement = null; return Promise.resolve(); },
    exitPointerLock() { doc.pointerLockElement = null; },
  };
  function makeEl(id, tag) {
    const set = new Set();
    const l = {};
    const el = {
      id, tagName: (tag || 'div').toUpperCase(), style: {}, textContent: '', value: '', checked: false,
      disabled: false, tabIndex: 0, className: '', onclick: null, srcObject: null, parentNode: null,
      classList: {
        add: (c) => set.add(c), remove: (c) => set.delete(c), contains: (c) => set.has(c),
        toggle(c, on) { if (on === undefined) on = !set.has(c); if (on) set.add(c); else set.delete(c); return on; },
      },
      addEventListener(t, f) { (l[t] = l[t] || []).push(f); },
      removeEventListener(t, f) { const a = l[t]; if (a) { const i = a.indexOf(f); if (i >= 0) a.splice(i, 1); } },
      _l: l,
      focus() {}, blur() {}, setAttribute() {}, removeAttribute() {}, appendChild() {}, insertBefore() {}, removeChild() {},
      replaceWith() {},
      transferControlToOffscreen() { return { offscreen: true }; },
      play() { return Promise.resolve(); }, pause() {},
      requestFullscreen() { doc.fullscreenElement = el; return Promise.resolve(); },
      requestPointerLock() { doc.pointerLockElement = el; return Promise.resolve(); },
    };
    // an iPhone: no element fullscreen anywhere, only the video element's native player
    if (o.iphone) {
      delete el.requestFullscreen;
      if (el.tagName === 'VIDEO') el.webkitEnterFullscreen = () => { el.webkitDisplayingFullscreen = true; };
    }
    return el;
  }
  const els = {};
  doc.getElementById = (id) => (els[id] = els[id] || makeEl(id, id === 'canvas' ? 'canvas' : id === 'video' ? 'video' : 'div'));
  const otp = [0, 1, 2, 3, 4, 5].map((i) => makeEl('otp' + i, 'input'));
  doc.querySelectorAll = (sel) => (sel === '#otp input' ? otp : []);
  doc.querySelector = () => makeEl('');
  doc.createElement = (tag) => makeEl('', tag);
  doc.body = makeEl('body');
  doc.documentElement = makeEl('html');

  const store = (init) => {
    const m = new Map(Object.entries(init || {}));
    return { getItem: (k) => (m.has(k) ? m.get(k) : null), setItem: (k, v) => m.set(k, String(v)), removeItem: (k) => m.delete(k) };
  };
  const url = new URL('https://192.168.1.5:8443/?id=ABCDEFGHJKLM&name=Phone&otp=123456&fs=1');
  const location = { href: url.href, protocol: url.protocol, search: url.search, hash: '' };

  class FakePC {
    constructor(cfg) {
      this.cfg = cfg; this.closed = false; this.iceGatheringState = 'complete';
      this.connectionState = 'new'; this.iceConnectionState = 'new'; this.localDescription = null; this.channels = [];
      log.pcs.push(this);
    }
    addTransceiver() { return { receiver: {} }; }
    createDataChannel(label) {
      const ch = { label, readyState: 'connecting', bufferedAmount: 0, binaryType: '', send() { log.errors.push('a data channel sent'); }, close() { this.readyState = 'closed'; } };
      this.channels.push(ch);
      return ch;
    }
    createOffer() { return Promise.resolve({ type: 'offer', sdp: 'v=0\r\n' }); }
    setLocalDescription(d) { this.localDescription = d; return Promise.resolve(); }
    setRemoteDescription(d) { this.remoteDescription = d; return Promise.resolve(); }
    addEventListener() {} removeEventListener() {}
    getStats() { return Promise.resolve(new Map()); }
    close() { this.closed = true; this.connectionState = 'closed'; }
  }
  function RTCRtpReceiver() {}
  RTCRtpReceiver.prototype.transform = null;
  class FakeWorker {
    constructor(u) { this.url = u; this.terminated = false; this.posted = []; log.workers.push(this); }
    postMessage(m) { this.posted.push(m); }
    terminate() { this.terminated = true; }
  }
  const resp = (status, body, headers) => ({
    ok: status >= 200 && status < 300, status,
    headers: { get: (k) => (headers && headers[k]) || null },
    text: () => Promise.resolve(body),
    json: () => Promise.resolve(JSON.parse(body)),
  });
  const JOIN = 'b'.repeat(32);
  let reconfigEpoch = 1;
  const whepScript = o.whep || [];
  let whepCalls = 0;
  function fetchFake(u, init) {
    const method = (init && init.method) || 'GET';
    log.fetches.push({ url: u, method, at: clock });
    const path = String(u).split('?')[0];
    if (path === '/net-config') return Promise.resolve(resp(200, JSON.stringify({ trickleIce: false, httpsPort: 8443 })));
    if (path === '/ice-config') return Promise.resolve(resp(200, JSON.stringify({ iceServers: [], locality: 'same-network' })));
    if (path === '/transform-worker.js') return Promise.resolve(resp(200, '// worker'));
    if (path === '/reconfig') return Promise.resolve(resp(200, JSON.stringify({ epoch: reconfigEpoch, kick: 1, audioSink: '' })));
    if (path === '/whep') {
      const what = whepScript[Math.min(whepCalls++, whepScript.length - 1)] || 'ok';
      if (what === 'hang') {
        return new Promise((_, reject) => {
          init.signal.addEventListener('abort', () => { const e = new Error('The operation was aborted.'); e.name = 'AbortError'; reject(e); });
        });
      }
      if (what === 500) return Promise.resolve(resp(500, 'join failed: virtual display 0 did not attach within timeout'));
      return Promise.resolve(resp(200, 'v=0\r\nanswer\r\n', { 'X-Join-Id': JOIN, 'X-Device-Token': 'tok' }));
    }
    return Promise.resolve(resp(404, ''));
  }

  const nav = {
    userAgent: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/154', platform: o.platform || 'Win32', maxTouchPoints: 0, onLine: true,
    wakeLock: {
      request() {
        log.wakeRequests++;
        const s = { released: false, l: [], addEventListener(t, f) { this.l.push(f); }, release() { this.released = true; this.l.forEach((f) => f()); return Promise.resolve(); } };
        log.sentinels.push(s);
        return Promise.resolve(s);
      },
    },
    sendBeacon: (u) => { log.beacons.push(u); return true; },
    keyboard: { locked: false, lock() { this.locked = true; return Promise.resolve(); }, unlock() { this.locked = false; } },
    mediaDevices: { enumerateDevices: () => Promise.resolve([]) },
  };

  // only iOS WebKit defines navigator.standalone
  if (o.iphone) nav.standalone = !!o.standalone;

  const SEAudio = {
    detectCapabilities: () => ({ webcodecsOpus: true, worklet: true }),
    verifyCapabilities: () => Promise.resolve({ webcodecsOpus: true, worklet: true, verified: true, wantsFallbackTrack: true, redundancy: true }),
    beginFromGesture: () => { log.audio.push('begin'); return Promise.resolve(true); },
    startPicker: () => Promise.resolve({ result: 'unsupported' }),
    prepareOutputsForJoin: () => Promise.resolve({ action: 'none', reason: 'test' }),
    teardown: () => log.audio.push('teardown'),
    endSession: () => log.audio.push('endSession'),
    setSpeakerIdentity() {}, postOutputs() {}, getSyncInfo: () => ({}), degradeFlags: () => 0, sinkTier: () => 0,
    suppressedCount: () => 0, resume: () => Promise.resolve(true), setHostClockOffset() {}, noteOutputChoiceRequest() {},
    setSpeaker: () => Promise.resolve(true), attachDataChannel() {}, noteFallbackStream() {}, setVideoDelay() {}, _enumPromise: null,
  };

  const quiet = { log() {}, warn() {}, info() {}, debug() {}, error() {} };
  const ctx = {
    console: quiet,
    setTimeout: (fn, ms, ...a) => addTimer(fn, ms, 0, a),
    setInterval: (fn, ms, ...a) => addTimer(fn, Math.max(1, +ms || 0), Math.max(1, +ms || 0), a),
    clearTimeout: kill, clearInterval: kill,
    requestAnimationFrame: () => 0,
    performance: { now: () => clock, timeOrigin: 1_700_000_000_000 },
    TextEncoder, URL, Blob, AbortController,
    MessageChannel: class { constructor() { this.port1 = { postMessage() {} }; this.port2 = {}; } },
    PointerEvent: class { getCoalescedEvents() { return []; } },
    RTCPeerConnection: FakePC, RTCRtpReceiver, RTCRtpScriptTransform: class { constructor(w, opt) { this.w = w; this.opt = opt; } },
    VideoDecoder: function () {}, Worker: FakeWorker,
    HTMLMediaElement: Object.assign(function () {}, { prototype: {} }),
    fetch: fetchFake,
    navigator: nav, document: doc, location,
    history: { replaceState() {} },
    localStorage: store(Object.assign({ 'se.compat.hideLimited': '1' }, o.storage)), sessionStorage: store(),
    crypto: { getRandomValues: (b) => b },
    matchMedia: () => ({ matches: false, addEventListener() {} }),
    screen: Object.assign({ width: 1280, height: 720, refreshRate: 60, orientation: { type: 'landscape-primary', addEventListener() {} } }, o.screen),
    isSecureContext: true, crossOriginIsolated: true, devicePixelRatio: 1,
    innerWidth: (o.inner && o.inner.width) || 1280, innerHeight: (o.inner && o.inner.height) || 720,
    addEventListener: (t, f) => W.add(t, f), removeEventListener: (t, f) => W.remove(t, f),
    open() {},
    __SAME_DEVICE__: false,
    SEAudio,
  };
  ctx.window = ctx;
  ctx.self = ctx;
  vm.createContext(ctx);
  vm.runInContext(INPUT_JS, ctx, { filename: 'input.js' });
  vm.runInContext(SCRIPT, ctx, { filename: 'index.html' });
  const read = (expr) => vm.runInContext(expr, ctx);

  async function advance(ms) {
    const until = clock + ms;
    for (;;) {
      let next = null;
      for (const t of timers) if (t.live && t.at <= until && (!next || t.at < next.at)) next = t;
      if (!next) break;
      clock = next.at;
      if (next.every) next.at += next.every; else next.live = false;
      try { next.fn(); } catch (e) { log.errors.push('timer threw: ' + e.message); }
      await realFlush();
    }
    clock = until;
    await realFlush();
  }
  async function connect() {
    const form = els.joinForm;
    const submit = form._l.submit[0];
    await submit({ preventDefault() {} });
    await realFlush();
  }
  // fires the page's own window listeners with a Ctrl+C keydown and reports whether any of them
  // swallowed it: a swallowed one means no copying and no typing on the form
  function ctrlCBlocked() {
    let prevented = false;
    const e = {
      type: 'keydown', code: 'KeyC', key: 'c', keyCode: 67, ctrlKey: true, shiftKey: false, altKey: false, metaKey: false,
      repeat: false, isComposing: false, location: 0, getModifierState: () => false,
      preventDefault() { prevented = true; }, stopPropagation() {},
    };
    for (const f of (winListeners.keydown || []).slice()) f(e);
    return prevented;
  }
  const lastPc = () => log.pcs[log.pcs.length - 1];
  const lastWorker = () => log.workers[log.workers.length - 1];
  async function goLive() {
    const pc = lastPc();
    pc.iceConnectionState = 'connected';
    pc.oniceconnectionstatechange();
    pc.connectionState = 'connected';
    pc.onconnectionstatechange();
    const w = lastWorker();
    w.onmessage({ data: { type: 'transformstart', transport: 'script', hasSendKey: true } });
    w.onmessage({ data: { type: 'rendered' } });
    await realFlush();
    await advance(200);
  }
  function state() {
    const join = els.join;
    return {
      formShown: join.style.display !== 'none' && !join.classList.contains('hide'),
      loading: els.loading.classList.contains('show'),
      error: els.errorMessage.textContent,
      connectEnabled: !els.connect.disabled,
      wakeHeld: log.sentinels.some((s) => !s.released),
      beforeunload: (winListeners.beforeunload || []).length,
      fullscreen: !!doc.fullscreenElement,
      pointerLock: !!doc.pointerLockElement,
      keyboardLock: nav.keyboard.locked,
      copyBlocked: ctrlCBlocked(),
      live: read('isLive'),
      intervals: timers.filter((t) => t.live && t.every).length,
      pcsOpen: log.pcs.filter((p) => !p.closed).length,
      workersAlive: log.workers.filter((w) => !w.terminated).length,
    };
  }
  return { log, read, advance, connect, goLive, state, ctrlCBlocked, winListeners, lastPc, JOIN, els, ctx, bumpEpoch: () => { reconfigEpoch++; } };
}

// The pre-join state every failure must land back on, with its message.
async function assertRestored(p, what, errRe) {
  const s = p.state();
  if (errRe.test(s.error)) ok(what + ': the form says why (' + s.error.slice(0, 70) + ')');
  else fail(what + ': error text = ' + JSON.stringify(s.error));
  eq(what + ': join form shown, spinner gone', s.formShown + '/' + s.loading, 'true/false');
  eq(what + ': Connect enabled', s.connectEnabled, true);
  eq(what + ': Ctrl+C reaches the page (copy and typing work)', s.copyBlocked, false);
  eq(what + ': wake lock released', s.wakeHeld, false);
  eq(what + ': no beforeunload prompt', s.beforeunload, 0);
  eq(what + ': fullscreen / pointer lock / keyboard lock', s.fullscreen + '/' + s.pointerLock + '/' + s.keyboardLock, 'false/false/false');
  eq(what + ': not live', s.live, false);
  eq(what + ': peer connections open / transform workers alive', s.pcsOpen + '/' + s.workersAlive, '0/0');
  eq(what + ': the audio session was ended', p.log.audio.includes('endSession'), true);
  // Nothing armed by the join may still be running: a minute later no interval is live and
  // nothing has gone to the host.
  const before = p.log.fetches.length;
  await p.advance(60000);
  const s2 = p.state();
  const later = p.log.fetches.slice(before).map((f) => f.method + ' ' + f.url);
  eq(what + ': live intervals a minute later', s2.intervals, 0);
  eq(what + ': requests to the host in the following minute', later.length ? later.join(', ') : 0, 0);
  eq(what + ': still restored a minute later (form, copy, wake lock)', s2.formShown + '/' + s2.copyBlocked + '/' + s2.wakeHeld, 'true/false/false');
  eq(what + ': page errors', p.log.errors.length ? p.log.errors.join(' | ') : 0, 0);
}

// 1. A 500 from /whep, and the same-tier retry refused too.
{
  const p = bootPage({ whep: [500, 500] });
  await p.advance(10);
  eq('before Connect: Ctrl+C reaches the page', p.ctrlCBlocked(), false);
  await p.connect();
  await p.advance(100);
  const posts = p.log.fetches.filter((f) => f.url === '/whep').length;
  eq('500 twice: exactly two offers (one same-tier retry, no <video> demotion)', posts, 2);
  eq('500: no /leave (the join is kept for a reconnect)', p.log.beacons.length, 0);
  await assertRestored(p, '500 from /whep', /^WHEP POST failed: 500 join failed: virtual display 0 did not attach/);
}

// 2. The 10 s answer deadline, and the retry's answer lost as well.
{
  const p = bootPage({ whep: ['hang', 'hang'] });
  await p.advance(10);
  await p.connect();
  eq('while connecting: Ctrl+C still reaches the page (input is armed only once live)', p.ctrlCBlocked(), false);
  eq('while connecting: no wake lock, no leave prompt yet', p.state().wakeHeld + '/' + p.state().beforeunload, 'false/0');
  await p.advance(10500);
  eq('after the first deadline: the retry is in flight', p.log.fetches.filter((f) => f.url === '/whep').length, 2);
  await p.advance(10500);
  eq('lost answer twice: two offers, no third on <video>', p.log.fetches.filter((f) => f.url === '/whep').length, 2);
  await assertRestored(p, 'answer timeout ×2', /^The host did not answer in time\.$/);
}

// 3. A successful join arms everything at the first painted frame; pagehide gives it all back.
{
  const p = bootPage({ whep: ['ok'] });
  await p.advance(10);
  await p.connect();
  await p.advance(50);
  const pre = p.state();
  eq('answered, not yet painting: Ctrl+C reaches the page', pre.copyBlocked, false);
  eq('answered, not yet painting: wake lock / leave prompt / fullscreen', pre.wakeHeld + '/' + pre.beforeunload + '/' + pre.fullscreen, 'false/0/false');
  await p.goLive();
  const live = p.state();
  eq('live: the stage owns the keyboard (Ctrl+C goes to the host)', live.copyBlocked, true);
  eq('live: wake lock held', live.wakeHeld, true);
  eq('live: beforeunload prompt armed', live.beforeunload, 1);
  eq('live: fullscreen + pointer lock + keyboard lock', live.fullscreen + '/' + live.pointerLock + '/' + live.keyboardLock, 'true/true/true');
  eq('live: form hidden', live.formShown, false);
  eq('live: one peer connection open', live.pcsOpen, 1);
  for (const f of (p.winListeners.pagehide || []).slice()) f({ persisted: true });
  await p.advance(10);
  eq('pagehide: one /leave beacon for the join', p.log.beacons.join(','), '/leave?join=' + p.JOIN);
  await assertRestored(p, 'pagehide after a live session', /^$/);
}

// 4. pagehide in the other states a page can be closed in.
{
  const p = bootPage({ whep: ['ok'] });
  await p.advance(10);
  await p.connect();
  await p.advance(50);
  for (const f of (p.winListeners.pagehide || []).slice()) f({ persisted: false });
  await p.advance(10);
  eq('pagehide answered, no frame yet: one /leave beacon', p.log.beacons.join(','), '/leave?join=' + p.JOIN);
  eq('and the peer connection is closed', p.state().pcsOpen, 0);
  const q = bootPage({});
  await q.advance(10);
  for (const f of (q.winListeners.pagehide || []).slice()) f({ persisted: false });
  eq('pagehide before any join sends nothing', q.log.beacons.length, 0);
}

// 5. A renegotiation that fails for good: live, the host bumps the epoch, the new offer is refused
// twice. It must not end on a frozen frame with the wake lock, fullscreen and the leave prompt
// still held, kept "alive" by a /reconfig poll the host keeps answering.
{
  const p = bootPage({ whep: ['ok', 500, 500] });
  await p.advance(10);
  await p.connect();
  await p.advance(50);
  await p.goLive();
  eq('renegotiation: live before the bump', p.state().live + '/' + p.state().wakeHeld, 'true/true');
  p.bumpEpoch();
  await p.advance(3000);
  eq('renegotiation: the bump re-offered, and the refusal was retried once', p.log.fetches.filter((f) => f.url === '/whep').length, 3);
  eq('renegotiation: no /leave (the join is kept)', p.log.beacons.length, 0);
  await assertRestored(p, 'failed renegotiation', /^WHEP POST failed: 500/);
}

// attemptId: one per page load, on every offer
{
  const m = /var attemptId = (\(function \(\) \{[\s\S]*?\}\)\(\));/.exec(page);
  if (!m) fail('attemptId not found');
  else {
    const withCrypto = vm.runInContext(m[1], vm.createContext({ Uint8Array, Math, window: { crypto: { getRandomValues: (b) => { for (let i = 0; i < b.length; i++) b[i] = i * 17; return b; } } } }));
    eq('attemptId from crypto', withCrypto, '00112233445566778899aabbccddeeff');
    const without = vm.runInContext(m[1], vm.createContext({ Uint8Array, Math, window: {} }));
    if (/^[0-9a-f]{32}$/.test(without)) ok('attemptId without crypto is still 32 hex digits');
    else fail('attemptId without crypto = ' + without);
  }
  const body = takeFunction('start');
  if (/attemptId: attemptId,/.test(body) && /joinId: currentJoinId,/.test(body)) ok('every offer carries the attempt id beside the join id');
  else fail('the WHEP body does not carry attemptId');
}

// Which fullscreen option the join form offers. Where the Fullscreen API works the toggle is as it
// always was; where only the native video player can go fullscreen (iPhone Safari) the same
// checkbox becomes Force fullscreen, off by default; where neither exists nothing changes.
{
  const ctx = vm.createContext({});
  vm.runInContext(takeFunction('fullscreenOption') + '\nthis.f = fullscreenOption;', ctx);
  const fn = () => {};
  eq('Fullscreen API on the stage', ctx.f({ requestFullscreen: fn }, { webkitEnterFullscreen: fn }), 'api');
  eq('prefixed Fullscreen API on the stage', ctx.f({ webkitRequestFullscreen: fn }, {}), 'api');
  eq('only the native video player (iPhone)', ctx.f({}, { webkitEnterFullscreen: fn }), 'force');
  eq('neither', ctx.f({}, {}), 'none');
  eq('neither, no video element', ctx.f({}, null), 'none');

  const boxes = page.match(/type="checkbox" id="fullscreenMode"/g) || [];
  eq('one fullscreen checkbox on the form, never a second', boxes.length, 1);
  const optionSrc = page.slice(page.indexOf("const fullscreenOptionKind"), page.indexOf("document.getElementById('brand')"));
  if (/fullscreenOptionKind === 'force'[\s\S]*'Force fullscreen'[\s\S]*fullscreenModeInput\.checked = false;/.test(optionSrc)) ok('force mode relabels the toggle and starts it unchecked');
  else fail('force mode does not relabel the toggle and clear it');
  if (/fullscreenOptionKind === 'force'\) fullscreenModeInput\.checked = localStorage\.getItem\(FORCE_FULLSCREEN_KEY\) === '1'/.test(optionSrc)) ok('force mode is remembered under its own key, off unless saved on');
  else fail('force mode does not read its own remembered value');
  const startSrc = takeFunction('start');
  if (/const webcodecs = [^;]*&& !forceFullscreen;/.test(startSrc)) ok('a forced join renders through <video>, never WebCodecs');
  else fail('a forced join can still take the WebCodecs path');
  if (/forceFullscreen = fullscreenOptionKind === 'force' && fullscreenModeInput\.checked;/.test(page)) ok('only a checked Force fullscreen forces the <video> path');
  else fail('forceFullscreen is not tied to the checked force option');
}

// Force fullscreen on an iPhone. The page URL carries fs=1 (the plain toggle's default), which
// must not turn force on; a forced join renders through <video> and must still go live at ICE
// connect, with no WebCodecs frame ever arriving.
{
  const fresh = bootPage({ iphone: true });
  await fresh.advance(10);
  eq('iphone: the form offers Force fullscreen', fresh.read('fullscreenOptionKind'), 'force');
  eq('iphone: fs=1 in the URL leaves Force fullscreen off', fresh.els.fullscreenMode.checked, false);
  eq('iphone: an old saved force value is not read', bootPage({ iphone: true, storage: { 'se.forceFullscreen': '1' } }).els.fullscreenMode.checked, false);

  const p = bootPage({ iphone: true, storage: { 'se.forceFullscreenOptIn': '1' } });
  await p.advance(10);
  eq('iphone: a ticked Force fullscreen is remembered', p.els.fullscreenMode.checked, true);
  await p.connect();
  await p.advance(50);
  eq('forced join: no transform worker (the <video> path)', p.log.workers.length, 0);
  const pc = p.lastPc();
  pc.iceConnectionState = 'connected';
  pc.oniceconnectionstatechange();
  pc.connectionState = 'connected';
  pc.onconnectionstatechange();
  await p.advance(3000);
  const st = p.state();
  eq('forced join: live once ICE connects, not stuck on Connecting', st.live + '/' + st.loading, 'true/false');
}

// The size the host is asked for: always the device's screen. Without real fullscreen the picture
// fills the page's own box, laid out from the top-left corner at an explicit size.
{
  const size = (p) => p.read('joinData.width + "x" + joinData.height');
  const stageBox = (p) => p.els.stage.style.width + 'x' + p.els.stage.style.height;
  const desk = { screen: { width: 1920, height: 1080 }, inner: { width: 1280, height: 650 } };

  const fs = bootPage(desk);
  await fs.advance(10);
  fs.els.fullscreenMode.checked = true;
  await fs.connect();
  eq('real fullscreen: the host is asked for the screen', size(fs), '1920x1080');

  const win = bootPage(desk);
  await win.advance(10);
  win.els.fullscreenMode.checked = false;
  await win.connect();
  eq('fullscreen off: the host is still asked for the screen', size(win), '1920x1080');
  await win.goLive();
  eq('fullscreen off: the stage is sized to that box from the top-left', stageBox(win) + ' ' + win.els.stage.style.bottom, '1280pxx650px auto');

  const phone = { iphone: true, platform: 'iPhone', screen: { width: 428, height: 926, orientation: { type: 'portrait-primary', addEventListener() {} } }, inner: { width: 428, height: 745 } };
  const tab = bootPage(phone);
  await tab.advance(10);
  await tab.connect();
  eq('iPhone Safari tab: the host is asked for the screen', size(tab), '428x926');

  const app = bootPage(Object.assign({}, phone, { standalone: true, inner: { width: 428, height: 880 } }));
  await app.advance(10);
  await app.connect();
  eq('iPhone Home Screen app: the host is asked for the whole screen', size(app), '428x926');
  await app.goLive();
  eq('iPhone Home Screen app: the stage covers the whole screen', stageBox(app), '428pxx926px');
  eq('iPhone Home Screen app: the picture covers the whole screen', app.read('canvas.style.width + "x" + canvas.style.height'), '428pxx926px');
  // html and body (height 100%, overflow hidden) would otherwise clip the stage at innerHeight
  eq('iPhone Home Screen app: html and body are as tall as the stage', app.read('document.documentElement.style.height') + ' ' + app.read('document.body.style.height'), '926px 926px');
  eq('iPhone Safari tab: html and body keep the stylesheet height', tab.read('document.documentElement.style.height') || '', '');
}

// The fullscreen option on an iPhone: the Home Screen hint and the remote-control warning only
// where the page is clearly iOS, and no option at all inside a Home Screen web app.
{
  const desc = (p) => p.read('fullscreenOptionHidden') + ' ' + p.read('iosHomeScreenCapable');
  const tab = bootPage({ iphone: true });
  await tab.advance(10);
  eq('iPhone Safari tab: Force fullscreen shown, Home Screen hint on', desc(tab), 'false true');
  const shown = (p) => (p.els.fsToggle ? p.els.fsToggle.style.display || '' : '');
  eq('iPhone Safari tab: the option is visible', shown(tab), '');
  const app = bootPage({ iphone: true, standalone: true });
  await app.advance(10);
  eq('iPhone Home Screen app: the fullscreen option is hidden', app.els.fsToggle.style.display, 'none');
  eq('iPhone Home Screen app: the option stays off', app.els.fullscreenMode.checked, false);
  const ticked = bootPage({ iphone: true, standalone: true, storage: { 'se.forceFullscreenOptIn': '1' } });
  await ticked.advance(10);
  eq('iPhone Home Screen app: a saved force choice is ignored', ticked.els.fullscreenMode.checked, false);
  const desk = bootPage({});
  await desk.advance(10);
  eq('desktop: no Home Screen hint and the option is shown', desc(desk) + ' ' + shown(desk), 'false false ');
  if (/Force fullscreen[\s\S]{0,400}remote control \(touch and keyboard\) does not work/.test(page)) ok('force mode warns that remote control does not work');
  else fail('force mode warns that remote control does not work: warning text missing');
}

if (failures) { console.log(failures + ' failure(s)'); process.exit(1); }
console.log('all client join assertions passed');

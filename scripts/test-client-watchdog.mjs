// The WebCodecs frame watchdog must never demote a session that is already painting.
//
// index.html arms the 5 s "no WebCodecs frame" watchdog from two handlers (ICE 'connected' /
// 'completed' and the peer connection's own 'connected'), and the worker's 'rendered' post,
// sent exactly once per session, is what clears it. Either handler can fire again after that
// post, and a watchdog armed then is never cleared: `fallbackToVideo` would send a second
// offer and leave a healthy session on <video> for the rest of its life.
//
// The functions are lifted out of the page by name and run against a fake clock.
//
//   node scripts/test-client-watchdog.mjs [path/to/static]
import fs from 'node:fs';
import vm from 'node:vm';

const STATIC = process.argv.slice(2).filter((a) => a.charAt(0) !== '-')[0] || 'src-tauri/src/streamer/static';
const page = fs.readFileSync(STATIC + '/index.html', 'utf8');

let failures = 0;
const ok = (m) => console.log('ok  - ' + m);
const fail = (m) => { failures++; console.log('FAIL- ' + m); };

function hasFunction(name) {
  try { takeFunction(name); return true; } catch (_) { return false; }
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

const strip = (src) => src.replace(/\/\*[\s\S]*?\*\//g, '').split('\n').map((l) => l.replace(/\/\/.*$/, '')).join('\n');

// the wiring, checked on the source
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

// the behaviour, against a fake clock
function harness(withPending) {
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
    const loadingText = { textContent: 'Connecting' };
    const loadingEl = { classList: { remove() {} }, querySelector() { return loadingText; } };
    const joinEl = { classList: { add() {} }, style: {} };
    function fallbackToVideo(r) { calls.push('fallbackToVideo:' + r); }
    function retryWithLegacyTransform(r) { calls.push('retryWithLegacyTransform:' + r); }
    function clearReconfigWatchdog() {}
    function dropStaleCanvas() {}
    function startReconfigPoll() { calls.push('startReconfigPoll'); }
    function showError() {}
    function startLock() { calls.push('live'); }
    function noteSuppressed() {}
    function noteRefinedRefreshRate() {}
    function clampRefresh(r) { return r; }
    const REFRESH_REFINE_SAMPLES = 1;
    function getRefreshRate() { return { then() {} }; }
    const PICTURE_PENDING_LIMIT_MS = 45000;
    const RECONFIG_DEADLINE_MS = 15000;
    const LIVE_HOLD_MS = 1500;
    const PENDING_TEXT = 'Preparing the display';
    const CONNECTING_TEXT = 'Connecting';
    let picturePending = false;
    let pictureHeld = false;
    let liveHeldForPicture = false;
    let reconfigHeld = false;
    let pictureTimer = null;
    let reconfigWatchdog = null;
    let pendingAsked = false;
    let hostStateSeen = false;
    let liveHoldTimer = null;
    let kicked = false;
    let framesArrived = false;
    let decoderRestarted = false;
    const document = { visibilityState: 'visible' };
    let frameWatchdogHeld = false;
    let pageVisibilityHook = null;
    const worker = { postMessage(m) { calls.push('worker:' + m.type); } };
    function armReconfigWatchdog() {}
    let wakeResolve = null;
    let locked = false;
    const video = { srcObject: null, videoWidth: 0 };
    function acquireWakeLock() { calls.push('wakeLock'); return { then(ok) { wakeResolve = ok; } }; }
    function releaseWakeLock() { calls.push('wakeRelease'); }
    function linkLost(m) { calls.push('linkLost:' + m); }
    function restorePreJoinPage(m) { calls.push('linkLost:' + m); }
    let rearmPictureWatchdogs = function () { transformWatchdogArmed = false; armTransformWatchdog(); };
  `;
  const names = ['armFrameWatchdog', 'armTransformWatchdog', 'onStreamLive'];
  const pendingNames = ['setPicturePending', 'clearPicturePending', 'noteHostStateSeen', 'liveOnConnect',
    'setLoadingText', 'armReconfigWatchdog', 'clearReconfigWatchdog', 'linkLost', 'videoShowsFrames',
    'onFirstEncodedFrame', 'onDecoderGaveUp', 'onPageVisibility', 'tellWorkerVisibility'];
  const missing = withPending ? pendingNames.filter((n) => !hasFunction(n)) : [];
  if (missing.length) {
    fail('the pending-picture functions are missing from the page: ' + missing.join(', '));
    return null;
  }
  const body = names.concat(withPending ? pendingNames : []).map(takeFunction).join('\n');
  vm.runInContext(prelude + body + `
    this.api = {
      iceConnected: () => armTransformWatchdog(),
      pcConnected: () => armTransformWatchdog(),
      rendered: () => { webcodecsPainted = true; if (typeof setPicturePending === 'function' && picturePending) setPicturePending(false); onStreamLive(); },
      teardown: () => { if (typeof clearPicturePending === 'function') clearPicturePending(); if (watchdog) { clearTimeout(watchdog); watchdog = null; } webcodecsPainted = false; },
      watchdog: () => watchdog,
      pong: (pending) => noteHostStateSeen(pending),
      videoConnected: () => liveOnConnect(),
      askedForPending: () => { pendingAsked = true; },
      reconfigure: () => { reconfiguring = true; armReconfigWatchdog(); },
      iceFailed: () => linkLost('Connection failed.'),
      isLive: () => isLive,
      text: () => loadingText.textContent,
      firstFrame: () => onFirstEncodedFrame('key'),
      decoderGaveUp: (r) => onDecoderGaveUp(r),
      hide: () => { document.visibilityState = 'hidden'; onPageVisibility(); },
      // A worker made while the page is hidden, as the page's factory hands it over.
      newWorkerWhileHidden: () => {
        document.visibilityState = 'hidden';
        const seen = [];
        tellWorkerVisibility({ postMessage(m) { seen.push(m); } });
        document.visibilityState = 'visible';
        return seen;
      },
      show: () => { document.visibilityState = 'visible'; onPageVisibility(); },
      clearPending: () => clearPicturePending(),
      pending: () => picturePending,
      wakeResolves: (ok) => { if (wakeResolve) wakeResolve(ok); },
      videoPainted: () => { video.srcObject = {}; video.videoWidth = 1920; },
    };
  `, ctx);
  return ctx;
}

function demotions(ctx) { return ctx.calls.filter((c) => /^(fallbackToVideo|retryWithLegacyTransform)/.test(c)); }

{
  // ICE connected, first frame painted, then the peer connection's own 'connected' (or ICE
  // 'completed') arms the watchdog again
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

// A host that answered before its picture was ready (the PONG's "picture pending" bit). The
// bit only ever arrives in a PONG, and the first PONG answers a PING sent when the data channel
// opens, which is after ICE has connected. Every case below keeps that order.

function lost(h) { return h.calls.filter((c) => /^linkLost/.test(c)); }

{
  // Connected, then the first PONG says pending: the watchdog already armed stands down, and
  // nothing fires however long it lasts (short of the bound).
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.iceConnected();
    h.advance(300);
    h.api.pong(true);
    h.advance(20000);
    if (demotions(h).length === 0 && !h.api.watchdog()) ok('a pending picture holds the watchdog armed at connect');
    else fail('a pending picture was demoted: ' + JSON.stringify(h.calls));
    if (/Preparing the display/.test(h.api.text())) ok('the spinner says what it is waiting for');
    else fail('no text under the spinner while pending: ' + h.api.text());
    if (h.calls.indexOf('wakeLock') >= 0 && h.calls.indexOf('startReconfigPoll') >= 0) ok('the screen is kept on and /reconfig is polled while pending');
    else fail('pending does not keep the screen on or poll /reconfig: ' + JSON.stringify(h.calls));
    // Clearing it arms the watchdog as if the session had just connected.
    h.api.pong(false);
    if (h.api.watchdog()) ok('clearing the bit arms the watchdog');
    else fail('clearing the bit armed nothing');
    if (h.api.text() === 'Connecting') ok('and the spinner text goes back');
    else fail('the spinner text stayed: ' + h.api.text());
    h.advance(5001);
    if (demotions(h).length === 1) ok('and it then demotes a session that still paints nothing within 5 s');
    else fail('the re-armed watchdog did not fire: ' + JSON.stringify(h.calls));
  }
}

{
  // A picture that comes right after the bit clears is not demoted; a PONG that still says
  // pending after the first painted frame is ignored.
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.iceConnected();
    h.api.pong(true);
    h.advance(3000);
    h.api.rendered();
    h.api.pong(true);
    h.advance(60000);
    if (demotions(h).length === 0 && lost(h).length === 0) ok('a painting session is never put back on the clock by a late PONG');
    else fail('a painting session was demoted or ended: ' + JSON.stringify(h.calls));
  }
}

{
  // A host that never clears it: the page ends in its error state at the bound, once.
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.iceConnected();
    h.api.pong(true);
    h.advance(44999);
    if (lost(h).length) fail('the pending bound fired early');
    h.advance(2);
    const l = lost(h);
    if (l.length === 1 && demotions(h).length === 0 && /did not become ready/.test(l[0])) ok('a picture that never comes ends the session at the bound: ' + l[0]);
    else fail('the pending bound did not end the session exactly once: ' + JSON.stringify(h.calls));
    h.advance(60000);
    if (lost(h).length === 1) ok('and only once');
    else fail('the pending bound fired more than once');
  }
}

{
  // A real ICE failure during the wait keeps the page's own wording.
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.iceConnected();
    h.api.pong(true);
    h.advance(2000);
    h.api.iceFailed();
    const l = lost(h);
    if (l.length === 1 && l[0] === 'linkLost:Connection failed.') ok('a lost link while pending says the link was lost');
    else fail('a lost link while pending was reworded: ' + JSON.stringify(l));
  }
}

{
  // The browser's own decoder (<video>) goes live at ICE connect, before any PONG. A page that
  // told the host it may answer early holds that until the first PONG.
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.videoConnected();
    if (!h.api.isLive()) ok('the <video> path waits for the first PONG before going live');
    else fail('the <video> path went live before the host said whether the picture is pending');
    h.advance(300);
    h.api.pong(true);
    h.advance(20000);
    if (!h.api.isLive()) ok('and stays connecting while the PONG says pending');
    else fail('the <video> path went live over a pending picture');
    h.api.pong(false);
    if (h.api.isLive()) ok('and goes live when it clears');
    else fail('the <video> path never went live after the bit cleared');
  }
  // A first PONG that says nothing is pending releases it at once.
  const h2 = harness(true);
  if (h2) {
    h2.api.askedForPending();
    h2.api.videoConnected();
    h2.advance(200);
    h2.api.pong(false);
    if (h2.api.isLive()) ok('a first PONG with the bit clear lets the <video> path go live');
    else fail('a clear first PONG did not release the <video> path');
  }
  // No PONG at all: released after the hold.
  const h3 = harness(true);
  if (h3) {
    h3.api.askedForPending();
    h3.api.videoConnected();
    h3.advance(1501);
    if (h3.api.isLive()) ok('with no PONG the <video> path goes live after 1.5 s');
    else fail('the <video> path was held with no PONG ever arriving');
  }
  // a page that did not ask goes live at connect
  const h4 = harness(true);
  if (h4) {
    h4.api.videoConnected();
    if (h4.api.isLive()) ok('a page that did not ask goes live at connect, as before');
    else fail('the <video> path was held for a host that cannot answer early');
  }
}

{
  // A renegotiation in place while pending: its deadline is held and re-armed when the bit clears.
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.reconfigure();
    h.api.iceConnected();
    h.api.pong(true);
    h.advance(30000);
    if (lost(h).length === 0) ok('a rebuild deadline is held while pending');
    else fail('the rebuild deadline fired while pending: ' + JSON.stringify(h.calls));
    h.api.pong(false);
    h.advance(15001);
    const l = lost(h);
    if (l.length === 1) ok('and re-armed when the bit clears');
    else fail('the rebuild deadline was not re-armed: ' + JSON.stringify(h.calls));
  }
}

{
  // A teardown while pending leaves nothing behind: no watchdog, no bound, no held go-live.
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.iceConnected();
    h.api.pong(true);
    h.api.teardown();
    h.advance(60000);
    if (demotions(h).length === 0 && lost(h).length === 0 && !h.api.isLive()) ok('a teardown while pending cancels the bound and arms nothing');
    else fail('a teardown while pending left a timer behind: ' + JSON.stringify(h.calls));
  }
  // And the order inside the real teardown: the page's pending state goes before the input
  // module's reset, so that reset can never run the "cleared" path mid-teardown.
  const td = strip(takeFunction('teardownSession'));
  const a = td.indexOf('clearPicturePending()');
  const b = td.indexOf('RemoteInput.teardown()');
  if (a >= 0 && b >= 0 && a < b) ok('teardownSession clears the pending state before the input teardown');
  else fail('teardownSession clears the pending state after the input teardown, or not at all');
}

{
  // A rebuild held while pending and then torn down for another rebuild keeps its deadline.
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.reconfigure();
    h.api.iceConnected();
    h.api.pong(true);
    h.advance(5000);
    h.api.clearPending();
    h.advance(15001);
    if (lost(h).length === 1) ok('a held rebuild deadline is re-armed when the session is torn down mid-rebuild');
    else fail('a held rebuild deadline was lost in a teardown: ' + JSON.stringify(h.calls));
  }
}

{
  // The <video> path paints with a delayed PONG still saying pending: its own evidence of a
  // picture (a decoded frame) settles it at the bound instead of ending the session.
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.videoConnected();
    h.api.pong(true);
    h.api.videoPainted();
    h.advance(45001);
    if (lost(h).length === 0 && !h.api.pending()) ok('a painting <video> session is not ended by the bound');
    else fail('a painting <video> session was ended at the bound: ' + JSON.stringify(h.calls));
  }
}

{
  // A wake-lock request that resolves after the wait ended (the page not live) is given back.
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.iceConnected();
    h.api.pong(true);
    h.api.clearPending();
    const before = h.calls.filter((c) => c === 'wakeRelease').length;
    h.api.wakeResolves(true);
    if (h.calls.filter((c) => c === 'wakeRelease').length > before) ok('a late wake lock after the wait ended is released');
    else fail('a late wake lock was kept on the join form: ' + JSON.stringify(h.calls));
  }
}

{
  // The first keyframe arrives while the picture is still pending, after a long gap, and is not
  // painted. The watchdog, re-armed when the bit clears, restarts the decoder and asks for a
  // keyframe first; only if that does not paint either is the session demoted.
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.iceConnected();
    h.api.pong(true);
    h.advance(12000);
    h.api.firstFrame();
    h.advance(400);
    h.api.pong(false);
    h.advance(5001);
    if (demotions(h).length === 0 && h.calls.indexOf('worker:resetdecoder') >= 0) ok('frames arriving and none painting restarts the decoder before any demotion');
    else fail('a session with frames arriving was demoted without a decoder restart: ' + JSON.stringify(h.calls));
    h.api.rendered();
    h.advance(10000);
    if (demotions(h).length === 0) ok('and a picture after the restart keeps the WebCodecs path');
    else fail('a session that painted after the restart was demoted: ' + JSON.stringify(h.calls));
  }
  const h2 = harness(true);
  if (h2) {
    h2.api.askedForPending();
    h2.api.iceConnected();
    h2.api.pong(true);
    h2.api.firstFrame();
    h2.api.pong(false);
    h2.advance(5001);
    h2.advance(5001);
    const d = demotions(h2);
    if (d.length === 1 && h2.calls.filter((c) => c === 'worker:resetdecoder').length === 1) ok('a decoder that still paints nothing after one restart is demoted, once');
    else fail('the restart was repeated or the demotion missing: ' + JSON.stringify(h2.calls));
  }
}

{
  // The worker giving up on its decoder (a configuration that never completed, or a restart with
  // no way to ask for a keyframe) demotes at once and once; after a paint it changes nothing.
  const h = harness(true);
  if (h) {
    h.api.iceConnected();
    h.api.firstFrame();
    h.advance(2000);
    h.api.decoderGaveUp('the decoder did not start (configuration still pending)');
    const at = demotions(h).length;
    h.advance(20000);
    if (at === 1 && demotions(h).length === 1 && h.calls.indexOf('worker:resetdecoder') < 0) ok('a decoder that will not start demotes at once, once, with no restart');
    else fail('a decoder that will not start was not demoted at once and once: ' + JSON.stringify(h.calls));
  }
  const h2 = harness(true);
  if (h2) {
    h2.api.iceConnected();
    h2.api.firstFrame();
    h2.api.rendered();
    h2.api.decoderGaveUp('frames arrived, none painted, and no keyframe can be asked for');
    h2.advance(20000);
    if (demotions(h2).length === 0) ok('and a painting session is never demoted by it');
    else fail('a painting session was demoted by the worker giving up: ' + JSON.stringify(h2.calls));
  }
}

{
  // A hidden tab whose browser suspends decoding while timers run: frames arrive and nothing
  // paints for 20 s, then the tab is shown and paints. Nothing is restarted or demoted, and the
  // worker is told both times.
  const h = harness(true);
  if (h) {
    h.api.iceConnected();
    h.api.firstFrame();
    h.advance(1000);
    h.api.hide();
    h.advance(20000);
    h.api.show();
    h.advance(1500);
    h.api.rendered();
    h.advance(20000);
    const told = h.calls.filter((c) => c === 'worker:visibility').length;
    if (demotions(h).length === 0 && h.calls.indexOf('worker:resetdecoder') < 0 && told === 2) ok('a hidden tab that paints once shown is neither restarted nor demoted');
    else fail('a hidden tab was restarted or demoted: ' + JSON.stringify(h.calls));
  }
  // A worker created on a page that is already hidden is told so at once.
  const h3 = harness(true);
  if (h3) {
    const seen = h3.api.newWorkerWhileHidden();
    if (seen.length === 1 && seen[0].type === 'visibility' && seen[0].hidden === true) ok('a worker created on a hidden page is told it is hidden at once');
    else fail('a worker created while hidden was not told: ' + JSON.stringify(seen));
  }
  // Shown and still not painting: the clock starts again in full from the moment it is shown.
  const h2 = harness(true);
  if (h2) {
    h2.api.iceConnected();
    h2.api.firstFrame();
    h2.advance(4000);
    h2.api.hide();
    h2.advance(20000);
    h2.api.show();
    h2.advance(4900);
    const early = h2.calls.indexOf('worker:resetdecoder') >= 0;
    h2.advance(200);
    if (!early && h2.calls.indexOf('worker:resetdecoder') >= 0 && demotions(h2).length === 0) ok('after a hidden spell the no-picture clock runs its full 5 s from being shown');
    else fail('the clock did not start afresh on being shown: ' + JSON.stringify(h2.calls));
  }
}

{
  // The clock against a late first frame: armed when the bit cleared with no frame yet, it
  // restarts from the first frame instead of counting the time before it.
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.iceConnected();
    h.api.pong(true);
    h.api.pong(false);
    h.advance(4000);
    h.api.firstFrame();
    h.advance(4000);
    if (demotions(h).length === 0 && h.calls.indexOf('worker:resetdecoder') < 0) ok('the no-picture clock counts from the first frame, not from the bit clearing');
    else fail('a late first frame was given less than 5 s: ' + JSON.stringify(h.calls));
  }
}

{
  // A hidden tab changes nothing here: the hold and the bound run on the same timers.
  const h = harness(true);
  if (h) {
    h.api.askedForPending();
    h.api.iceConnected();
    h.api.pong(true);
    h.advance(30000);
    if (demotions(h).length === 0 && lost(h).length === 0) ok('a hidden tab while pending is held like a visible one');
    else fail('a hidden tab while pending fired something: ' + JSON.stringify(h.calls));
  }
}

{
  // an old host never sends the bit: the plain 5 s watchdog applies
  const h = harness(false);
  h.api.iceConnected();
  h.advance(5001);
  if (demotions(h).length === 1) ok('without the bit the watchdog behaves as before');
  else fail('a host without the bit changed the watchdog: ' + JSON.stringify(h.calls));
}

if (failures) { console.log(failures + ' failure(s)'); process.exit(1); }
console.log('all passed');

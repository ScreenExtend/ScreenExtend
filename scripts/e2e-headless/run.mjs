// Headless-Chrome join + latency probe. No input is injected anywhere: every page action is a
// DOM call through Runtime.evaluate, and motion comes from mover.ps1 (a tiny moving window).
import { spawn, execFileSync } from 'node:child_process';
import path from 'node:path';
import os from 'node:os';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const CHROME = process.env.CHROME || 'C:/Program Files/Google/Chrome/Application/chrome.exe';
const EXE = process.env.SE_EXE || 'C:/Users/sarve/.cargo/shared_target/ScreenExtend/release/ScreenExtend.exe';
const [HOST, SID, OTP] = process.argv.slice(2);
const ORIGIN = `https://${HOST}:8443`;
const URL_ = `${ORIGIN}/?id=${SID}&name=HeadlessProbe&otp=${OTP}&fs=0${process.env.DEBUG ? '&debug' : ''}`;
const PORT = 9333;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const out = { steps: [] };
const step = (name, v) => { out.steps.push({ name, ...v }); console.log('STEP', name, JSON.stringify(v)); };

// A fresh profile every run: a new browser, a new device token, nothing cached.
const PROFILE = path.join(os.tmpdir(), 'screenextend-e2e-profile');
fs.rmSync(PROFILE, { recursive: true, force: true });
const chrome = spawn(CHROME, [
  ...(process.env.HEADED ? ['--window-position=80,80'] : ['--headless=new']), `--remote-debugging-port=${PORT}`, `--user-data-dir=${PROFILE}`,
  '--ignore-certificate-errors', ...(process.env.NO_AUTOPLAY_FLAG ? [] : ['--autoplay-policy=no-user-gesture-required']),
  '--use-fake-device-for-media-stream', '--window-size=1280,720', '--no-first-run', ...(process.env.CHROME_FLAGS ? process.env.CHROME_FLAGS.split(' ') : []), // GPU: headless=new on Windows already runs ANGLE/D3D11 with hardware video decode (checked
  // with SystemInfo.getInfo), so --enable-gpu / --ignore-gpu-blocklist are opt-in, not defaults.
  ...(process.env.GPU_FLAGS ? ['--ignore-gpu-blocklist', '--enable-gpu'] : []), 'about:blank',
], { stdio: 'ignore' });
let chromeExited = false;
chrome.on('exit', () => { chromeExited = true; });

async function cdp(wsUrl) {
  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0; const pending = new Map(); const events = []; const hooks = [];
  ws.onmessage = (m) => {
    const d = JSON.parse(m.data);
    if (d.id && pending.has(d.id)) { const p = pending.get(d.id); pending.delete(d.id); d.error ? p.rej(new Error(JSON.stringify(d.error))) : p.res(d.result); }
    else if (d.method) { events.push(d); hooks.forEach((h) => h(d)); }
  };
  return {
    events,
    send: (method, params = {}) => new Promise((res, rej) => { const my = ++id; pending.set(my, { res, rej }); ws.send(JSON.stringify({ id: my, method, params })); setTimeout(() => { if (pending.delete(my)) rej(new Error('CDP timeout: ' + method)); }, 15000); }),
    on: (fn) => { hooks.push(fn); },
    raw: (o) => ws.send(JSON.stringify(o)),
    close: () => ws.close(),
  };
}

try {
  let ver = null;
  for (let i = 0; i < 50 && !ver; i++) { try { ver = await (await fetch(`http://127.0.0.1:${PORT}/json/version`)).json(); } catch { await sleep(200); } }
  if (!ver) throw new Error('chrome debugging port never answered');
  step('chrome-started', { browser: ver.Browser, pid: chrome.pid });
  const browser = await cdp(ver.webSocketDebuggerUrl);
  const tgt = await (await fetch(`http://127.0.0.1:${PORT}/json/new?about:blank`, { method: 'PUT' })).json();
  const page = await cdp(tgt.webSocketDebuggerUrl);
  const dialogs = [];
  page.on((d) => { if (d.method === 'Page.javascriptDialogOpening') { dialogs.push(d.params.type + ': ' + d.params.message); page.send('Page.handleJavaScriptDialog', { accept: true }).catch(() => {}); } });
  let rid = 900000;
  page.on((d) => {
    if (d.method === 'Target.attachedToTarget') page.raw({ id: ++rid, sessionId: d.params.sessionId, method: 'Runtime.enable' });
    if (d.method === 'Fetch.requestPaused') (async () => {
      const q = d.params;
      const body = await page.send('Fetch.getResponseBody', { requestId: q.requestId });
      const headers = (q.responseHeaders || []).concat([{ name: 'Cross-Origin-Embedder-Policy', value: 'require-corp' }]);
      await page.send('Fetch.fulfillRequest', { requestId: q.requestId, responseCode: q.responseStatusCode, responseHeaders: headers, body: body.base64Encoded ? body.body : Buffer.from(body.body).toString('base64') });
    })().catch((e) => console.log('fetch patch failed', e.message));
  });
  const net = {}; const T0 = Date.now();
  page.on((d) => { const q = d.params || {};
    if (d.method === 'Network.requestWillBeSent' && /\/whep/.test(q.request.url)) { net[q.requestId] = q.request.method; console.log('NET', new Date().toISOString().slice(11, 23), 'send', q.request.method, q.request.url.replace(/^https:\/\/[^/]+/, '').slice(0, 30)); }
    if (d.method === 'Network.responseReceived' && net[q.requestId]) console.log('NET', new Date().toISOString().slice(11, 23), 'resp', net[q.requestId], q.response.status, JSON.stringify(q.response.timing ? { dns: q.response.timing.dnsEnd, connect: q.response.timing.connectEnd, ssl: q.response.timing.sslEnd, sendEnd: q.response.timing.sendEnd, headers: q.response.timing.receiveHeadersEnd } : null), 'reused=' + q.response.connectionReused);
    if (d.method === 'Network.loadingFailed' && net[q.requestId]) console.log('NET', new Date().toISOString().slice(11, 23), 'failed', net[q.requestId], q.errorText, 'canceled=' + q.canceled);
  });
  await page.send('Network.enable');
  await page.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true });
  if (process.env.FIX_COEP) await page.send('Fetch.enable', { patterns: [{ urlPattern: '*-worker.js', requestStage: 'Response' }] });
  await page.send('Runtime.enable'); await page.send('Page.enable'); await page.send('Log.enable');
  const ev = async (expr, gesture = false) => {
    const r = await page.send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true, userGesture: gesture });
    if (r.exceptionDetails) throw new Error('page threw: ' + JSON.stringify(r.exceptionDetails.exception || r.exceptionDetails.text));
    return r.result.value;
  };

  // Observation only: keep the worker's own stats reply and every peer connection where the
  // harness can read them. addEventListener runs beside the page's onmessage, never instead.
  await page.send('Page.addScriptToEvaluateOnNewDocument', { source: `(() => {
    ${process.env.NO_SCRIPT_TRANSFORM ? 'delete window.RTCRtpScriptTransform;' : ''}
    ${process.env.NO_TRICKLE ? "const F = window.fetch; window.fetch = function (u, o) { const p = F.apply(this, arguments); return String(u).indexOf('/net-config') >= 0 ? p.then((r) => r.clone().json().then((j) => { j.trickleIce = false; return new Response(JSON.stringify(j), { status: 200, headers: { 'Content-Type': 'application/json' } }); })) : p; };" : ''}
    (function () { const C = AudioNode.prototype.connect; AudioNode.prototype.connect = function (d) { try { if (d instanceof AudioDestinationNode && !this.__tap) { const an = this.context.createAnalyser(); an.fftSize = 8192; an.__tap = true; C.call(this, an); (window.__ans = window.__ans || []).push(an); } } catch (_) {} return C.apply(this, arguments); }; })();
    const W = window.Worker; window.__vs = null; window.__wmsgs = [];
    window.Worker = function (u, o) { const w = new W(u, o); w.addEventListener('message', (m) => { const d = m.data || {}; if (d.type === 'videostats') window.__vs = d.stats; else if (d.type !== 'avsync' && d.type !== 'rendercount' && d.type !== 'size') window.__wmsgs.push(Math.round(performance.now()) + ' ' + d.type + (d.message ? ' ' + d.message : '') + (d.frameType ? ' ' + d.frameType : '')); }); return w; };
    window.Worker.prototype = W.prototype;
    const P = window.RTCPeerConnection; window.__pcs = [];
    window.RTCPeerConnection = function (c) { const pc = new P(c); window.__pcs.push(pc); window.__wmsgs.push(Math.round(performance.now()) + ' new-pc'); const tr0 = pc.addTransceiver; pc.addTransceiver = function (k, o) { const t = tr0.apply(this, arguments); if (k === 'video') { window.__vt = t; ${process.env.ONLY_PLID ? `try { t.setCodecPreferences(RTCRtpReceiver.getCapabilities('video').codecs.filter((c) => /rtx/i.test(c.mimeType) || (/h264/i.test(c.mimeType) && new RegExp('profile-level-id=${process.env.ONLY_PLID}').test(c.sdpFmtpLine || '')))); } catch (e) { window.__wmsgs.push('scp ' + e); }` : ''} const d = Object.getOwnPropertyDescriptor(RTCRtpReceiver.prototype, 'transform'); if (d && d.set) Object.defineProperty(t.receiver, 'transform', { get() { return d.get.call(this); }, set(v) { window.__wmsgs.push(Math.round(performance.now()) + ' transform-set ' + (v ? v.constructor.name : v)); d.set.call(this, v); } }); } return t; }; return pc; };
    window.RTCPeerConnection.prototype = P.prototype;
    const srd = P.prototype.setRemoteDescription; P.prototype.setRemoteDescription = function (d) { try { (window.__sdps = window.__sdps || []).push(d && d.sdp); } catch (_) {} const self = this, args = arguments; return this.getStats().then((st) => { let f = null, p = null; st.forEach((x) => { if (x.type === 'inbound-rtp' && x.kind === 'video') { f = x.framesReceived; p = x.packetsReceived; } }); window.__wmsgs.push(Math.round(performance.now()) + ' SRD conn=' + self.connectionState + ' ice=' + self.iceConnectionState + ' dtls/pkts-before=' + p + ' frames-before=' + f); }, () => {}).then(() => srd.apply(self, args)).then((v) => { try { window.__wmsgs.push(Math.round(performance.now()) + ' after-SRD ' + self.getTransceivers().map((t) => t.mid + ':' + t.receiver.track.kind + ':' + (t === window.__vt ? 'ours' : 'other') + ':tf=' + !!t.receiver.transform + ':dir=' + t.currentDirection).join(' ')); } catch (e) { window.__wmsgs.push('after-SRD err ' + e); } return v; }); };
    for (const k of Object.getOwnPropertyNames(P)) { try { if (!(k in window.RTCPeerConnection)) window.RTCPeerConnection[k] = P[k]; } catch (_) {} }
  })();` });
  const inbound = () => ev(`(async () => { const pc = window.__pcs[window.__pcs.length - 1]; if (!pc) return null; const r = await pc.getStats(); let o = null; const cs = {}; r.forEach((s) => { if (s.type === 'codec') cs[s.id] = s.payloadType + ' ' + s.mimeType + ' ' + (s.sdpFmtpLine || ''); }); r.forEach((s) => { if (s.type === 'inbound-rtp' && s.kind === 'video') o = { ssrc: s.ssrc, codec: cs[s.codecId], impl: s.decoderImplementation, pkts: s.packetsReceived, lost: s.packetsLost, fr: s.framesReceived, dec: s.framesDecoded, key: s.keyFramesDecoded, pli: s.pliCount, fir: s.firCount, nack: s.nackCount }; }); return o; })()`).catch(() => null);
  await page.send('Page.navigate', { url: URL_ });
  for (let i = 0; i < 50; i++) { if (await ev('document.readyState === "complete" && !!document.getElementById("joinForm")').catch(() => false)) break; await sleep(200); }
  step('page-loaded', { title: await ev('document.title'), secure: await ev('isSecureContext'), coi: await ev('crossOriginIsolated') });

  // ---- permission dialog: ungranted state, then accept it through the browser protocol
  const micQ = 'navigator.permissions.query({name:"microphone"}).then(p=>p.state)';
  const before = await ev(micQ);
  // Every permission the grant does not name is *denied*, not left at prompt: granting only
  // audioCapture is what made navigator.wakeLock.request() reject with NotAllowedError.
  await browser.send('Browser.grantPermissions', { origin: ORIGIN, permissions: process.env.GRANT_ONLY_MIC ? ['audioCapture'] : ['audioCapture', 'wakeLockScreen', 'speakerSelection'] });
  const after = await ev(micQ);
  step('permission-grant', { before, after });

  // ---- what this Chrome actually offers the client page (report only; nothing here is exercised
  // against the host, and pointer lock is never requested)
  if (!process.env.NO_FEATURES) {
    const gpu = await browser.send('SystemInfo.getInfo').catch((e) => ({ error: e.message }));
    const fs = (gpu.gpu && gpu.gpu.featureStatus) || {};
    step('gpu', { devices: ((gpu.gpu && gpu.gpu.devices) || []).map((d) => d.deviceString || (d.vendorId + ':' + d.deviceId)), video_decode: fs.video_decode, gpu_compositing: fs.gpu_compositing, webgl: fs.webgl, rasterization: fs.rasterization, angle: ((gpu.gpu && gpu.gpu.auxAttributes) || {}).glImplementationParts, error: gpu.error });
    step('features', await ev(`(async () => {
      const o = {}; const t = (p) => p.then((x) => x, (e) => 'ERR ' + e.name + ': ' + e.message);
      o.coi = crossOriginIsolated; o.sab = typeof SharedArrayBuffer === 'function';
      o.scriptTransform = typeof RTCRtpScriptTransform === 'function';
      o.encodedStreams = typeof RTCRtpReceiver !== 'undefined' && 'createEncodedStreams' in RTCRtpReceiver.prototype;
      o.h264 = {};
      for (const hw of ['no-preference', 'prefer-hardware', 'prefer-software']) o.h264[hw] = await t(VideoDecoder.isConfigSupported({ codec: 'avc1.640028', optimizeForLatency: true, hardwareAcceleration: hw }).then((x) => x.supported));
      o.opus = await t(AudioDecoder.isConfigSupported({ codec: 'opus', sampleRate: 48000, numberOfChannels: 2 }).then((x) => x.supported));
      o.audioWorklet = typeof AudioWorkletNode === 'function';
      try { const ac = new AudioContext(); o.audioContextState = ac.state; await t(ac.resume()); o.audioContextAfterResume = ac.state; ac.close(); } catch (e) { o.audioContext = 'ERR ' + e.message; }
      o.wakeLock = await t(navigator.wakeLock ? navigator.wakeLock.request('screen').then((l) => { l.release(); return 'granted'; }) : Promise.resolve('absent'));
      o.wakeLockPerm = await t(navigator.permissions.query({ name: 'screen-wake-lock' }).then((p) => p.state));
      o.fullscreenEnabled = document.fullscreenEnabled; o.pointerLockApi = 'requestPointerLock' in Element.prototype;
      o.keyboardLockApi = !!(navigator.keyboard && navigator.keyboard.lock);
      o.setSinkId = 'setSinkId' in HTMLMediaElement.prototype; o.audioContextSetSinkId = 'setSinkId' in AudioContext.prototype;
      o.outputs = await t(navigator.mediaDevices.enumerateDevices().then((d) => d.filter((x) => x.kind === 'audiooutput').map((x) => x.label || '(unlabelled)')));
      o.autoplayPolicy = navigator.getAutoplayPolicy ? navigator.getAutoplayPolicy('mediaelement') : 'n/a';
      o.visibility = document.visibilityState;
      return o; })()`, true));
    if (process.env.FEATURES_ONLY) throw new Error('FEATURES_ONLY: stopping before the join');
  }

  // ---- page-level dialogs before the join
  const modals = [];
  const sweep = async () => {
    const hit = await ev(`(() => {
      const shown = (id) => { const m = document.getElementById(id); if (!m) return false; const s = getComputedStyle(m); return s.display !== 'none' && s.visibility !== 'hidden' && (m.classList.contains('show') || m.style.display === 'flex'); };
      if (shown('sameDeviceModal')) { document.getElementById('sameDeviceDismiss').click(); return 'sameDevice'; }
      if (shown('compatModal')) { document.getElementById('compatContinue').click(); return 'compat'; }
      if (shown('permModal')) { document.getElementById('permContinue').click(); return 'micCard'; }
      const h = document.getElementById('dismiss'); if (h && h.offsetParent) { h.click(); return 'httpsUpsell'; }
      return null; })()`, true);
    if (hit) modals.push(hit);
    return hit;
  };
  for (let i = 0; i < 10; i++) { await sweep(); await sleep(150); }
  const canJoin = await ev('!document.getElementById("connect").disabled');
  step('join-form', { modalsDismissed: [...modals], connectEnabled: canJoin });
  if (!canJoin) throw new Error('Connect stayed disabled: ' + await ev('document.getElementById("errorMessage").textContent'));

  const t0 = Date.now();
  await ev('document.getElementById("joinForm").requestSubmit()', true);
  if (process.env.EARLYMOVE) (async () => { for (let i = 0; i < 40; i++) { const code = await new Promise((r) => spawn('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(here, 'mover.ps1'), '12'], { stdio: 'ignore' }).on('exit', r)); if (code === 0) break; await sleep(300); } })();
  let stats = null;
  for (let i = 0; i < 150; i++) {
    await sweep();
    stats = await ev('window.SEVideo ? SEVideo.getStats() : null').catch(() => null);
    if (stats && i % 3 === 0) console.log('EARLY', Date.now() - t0, JSON.stringify({ vp: stats.videoPath, painted: stats.framesPainted, dropped: stats.framesDropped, decErr: stats.decodeErrors, keyReq: stats.keyRequests, tt: stats.transportTier, rt: stats.renderTier, hw: stats.decoderHardware, dec: stats.decode, a2p: stats.arrivalToPaint, deg: stats.degraded }), 'W', JSON.stringify(await ev('window.__vs && { in: __vs.framesIn, dec: __vs.framesDecoded, rend: __vs.framesRendered, unp: __vs.framesUnpainted, inFl: __vs.inFlight, wait: __vs.waitingForKey, cfg: __vs.configured, q: __vs.queue, kt: __vs.keyTier, sup: __vs.suppressed }').catch(() => null)), 'RTP', JSON.stringify(await inbound()));
    if (stats && stats.framesPainted > 0) break;
    await sleep(200);
  }
  const err = await ev('document.getElementById("errorMessage").textContent');
  if (!stats || !(stats.framesPainted > 0)) throw new Error('no frame painted in 30 s; page error: ' + JSON.stringify(err));
  step('connected', { msToFirstPaint: Date.now() - t0, modalsDismissed: modals, videoPath: stats.videoPath, audioPath: stats.audioPath });
  const labels = await ev('navigator.mediaDevices.enumerateDevices().then(d => d.filter(x => x.kind === "audiooutput").map(x => x.label))');
  step('permission-effect', { micStateNow: await ev(micQ), audioOutputLabels: labels });

  const audioProbe = () => ev(`(async () => {
    const o = { sync: window.SEAudio ? SEAudio.getSyncInfo() : null, taps: [] };
    for (const an of (window.__ans || [])) { const t = new Float32Array(an.fftSize); an.getFloatTimeDomainData(t); let q = 0; for (let i = 0; i < t.length; i++) q += t[i] * t[i];
      const f = new Float32Array(an.frequencyBinCount); an.getFloatFrequencyData(f); let k = 0; for (let i = 1; i < f.length; i++) if (f[i] > f[k]) k = i;
      o.taps.push({ rms: +Math.sqrt(q / t.length).toFixed(5), peakHz: Math.round(k * an.context.sampleRate / an.fftSize), peakDb: Math.round(f[k]), ctx: an.context.state }); }
    o.dc = []; for (const pc of (window.__pcs || [])) { const r = await pc.getStats(); r.forEach((x) => { if (x.type === 'data-channel') o.dc.push({ label: x.label, state: x.state, msgs: x.messagesReceived, bytes: x.bytesReceived }); if (x.type === 'inbound-rtp' && x.kind === 'audio') o.dc.push({ label: 'rtp-audio', msgs: x.packetsReceived, bytes: x.bytesReceived }); }); }
    return o; })()`);
  let tone = null; const audioSamples = [];
  if (process.env.AUDIO) {
    try { console.log('AUDIO-ON', (() => { const j = JSON.parse(execFileSync(EXE, ['status', '--json'], { encoding: 'utf8' })).telemetry.joins.filter((x) => x.name === 'HeadlessProbe'); const tok = j.length ? j[j.length - 1].token : HOST; return execFileSync(EXE, ['devices', 'set', tok, '--audio', 'on'], { encoding: 'utf8' }); })().trim().slice(0, 300)); } catch (e) { console.log('AUDIO-ON failed', (e.stdout || '') + (e.stderr || '') + e.message); }
    audioSamples.push({ phase: 'silence-before', t: Date.now(), ...(await audioProbe()) }); await sleep(3000);
    audioSamples.push({ phase: 'silence-3s', t: Date.now(), ...(await audioProbe()) });
    tone = spawn('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(here, 'tone.ps1'), '26'], { stdio: 'ignore' });
  }

  // ---- latency: 20 s with a small window moving on the virtual display
  const mover = spawn('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', path.join(here, 'mover.ps1'), '20'], { stdio: ['ignore', 'pipe', 'pipe'] });
  let moverOut = ''; mover.stdout.on('data', (d) => { moverOut += d; }); mover.stderr.on('data', (d) => { moverOut += d; });
  await sleep(3000);
  const a = await ev('SEVideo.getStats()'); const ta = Date.now();
  const samples = [];
  for (let i = 0; i < 15; i++) { await sleep(1000); const s = await ev('SEVideo.getStats()'); samples.push({ rtt: s.rttMs, h2p: s.hostToPaint, dec: s.decode, a2p: s.arrivalToPaint }); if (process.env.AUDIO && i % 3 === 2) audioSamples.push({ phase: 'tone', t: Date.now(), ...(await audioProbe()) }); }
  const b = await ev('SEVideo.getStats()'); const tb = Date.now();
  await new Promise((r) => mover.on('exit', r));
  out.latency = {
    mover: moverOut.trim(),
    paintedFps: +((b.framesPainted - a.framesPainted) / ((tb - ta) / 1000)).toFixed(1),
    framesDropped: b.framesDropped - a.framesDropped, decodeErrors: b.decodeErrors, keyRequests: b.keyRequests,
    rttMs: b.rttMs, clockValid: b.clockValid, decoderHardware: b.decoderHardware,
    hostToPaint: b.hostToPaint, decode: b.decode, arrivalToPaint: b.arrivalToPaint,
    samples, finalStats: b,
  };
  console.log('LATENCY', JSON.stringify(out.latency, null, 1));
  if (process.env.AUDIO) { await new Promise((r) => tone.on('exit', r)); await sleep(1500); audioSamples.push({ phase: 'silence-after', t: Date.now(), ...(await audioProbe()) }); for (const a of audioSamples) console.log('AUDIO', JSON.stringify(a)); }
  {
    const L = out.latency, r2 = (x) => (x == null ? null : +x.toFixed(2)), pc = (o) => (o ? { p50: r2(o.p50), p90: r2(o.p90), p99: r2(o.p99), max: r2(o.max) } : null);
    const R = { video: { path: L.finalStats.videoPath, firstPaintMs: out.steps.find((x) => x.name === 'connected').msToFirstPaint, paintedFps: L.paintedFps, hostToPaintMs: pc(L.hostToPaint), decodeMs: pc(L.decode), arrivalToPaintMs: pc(L.arrivalToPaint), framesDropped: L.framesDropped, decodeErrors: L.decodeErrors, hardwareDecoder: L.decoderHardware, rttMs: r2(L.rttMs) } };
    const fail = [];
    if (R.video.path !== 1) fail.push('video not on the WebCodecs path (videoPath=' + R.video.path + ')');
    if (!L.decode) fail.push('no decode timings'); if (L.decodeErrors) fail.push('decode errors'); if (!(L.paintedFps >= 30)) fail.push('painted fps below 30');
    if (process.env.AUDIO) {
      const tones = audioSamples.filter((a) => a.phase === 'tone'), quiet = audioSamples.filter((a) => a.phase !== 'tone'), last = audioSamples[audioSamples.length - 1];
      const tap = (a) => (a.taps[a.taps.length - 1] || { rms: 0, peakHz: 0 }), dcn = (a) => (a.dc.filter((d) => d.label === 'audio').map((d) => d.msgs).pop() || 0);
      const t0 = tones[0], t1 = tones[tones.length - 1];
      R.audio = { path: L.finalStats.audioPath, tonePeakHz: tap(t1).peakHz, toneRms: tap(t1).rms, silenceRmsBefore: tap(quiet[0]).rms, silenceRmsAfter: tap(last).rms, packetsPerSec: t0 && t1 !== t0 ? Math.round((dcn(t1) - dcn(t0)) / ((t1.t - t0.t) / 1000)) : null,
        underruns: last.sync.underruns, overruns: last.sync.overruns, concealed: last.sync.concealed, bufferDepthMs: [Math.min(...tones.map((a) => a.sync.depthMs)), Math.max(...tones.map((a) => a.sync.depthMs))], bufferTargetMs: last.sync.targetMs,
        captureToEnqueueMs: r2(last.sync.preEnqueueMs), outputLatencyMs: r2(last.sync.outputLatencyMs), avResidualMs: r2(last.sync.residualOffsetMs), transport: last.sync.transport };
      if (R.audio.path !== 1) fail.push('audio not on the worklet+shared-memory path (audioPath=' + R.audio.path + ')');
      if (Math.abs(R.audio.tonePeakHz - 440) > 12) fail.push('440 Hz tone not heard at the client (peak ' + R.audio.tonePeakHz + ' Hz)');
      if (!(R.audio.toneRms > 0.01)) fail.push('tone level too low'); if (R.audio.silenceRmsAfter > 0.005) fail.push('client not silent after the tone');
      if (R.audio.underruns || R.audio.concealed) fail.push('audio underruns/concealment');
    }
    R.pass = fail.length === 0; R.failures = fail; out.result = R;
    console.log('RESULT', JSON.stringify(R, null, 1));
  }
  try { out.hostStatus = JSON.parse(execFileSync(EXE, ['status', '--json'], { encoding: 'utf8' })).telemetry; console.log('HOST', JSON.stringify(out.hostStatus, null, 1)); } catch (e) { console.log('HOST status failed', e.message); }
  const logs = page.events.filter((e) => e.method === 'Log.entryAdded').map((e) => 'LOG ' + e.params.entry.level + ': ' + e.params.entry.text + ' ' + (e.params.entry.url || '')).concat(page.events.filter((e) => e.method === 'Runtime.exceptionThrown').map((e) => 'EXC ' + JSON.stringify(e.params.exceptionDetails).slice(0, 400)), page.events.filter((e) => e.method === 'Runtime.consoleAPICalled').map((e) => e.params.type + ': ' + e.params.args.map((x) => x.value || x.description || '').join(' ').slice(0, 300)));
  if (process.env.SDP) console.log('SDP', await ev('JSON.stringify(window.__sdps)').catch(() => null));
  console.log('WORKER-MSGS', JSON.stringify(await ev('window.__wmsgs').catch(() => null)));
  console.log('PAGE-WARNINGS', JSON.stringify(logs, null, 1));

  // ---- leave cleanly, then shut the browser down
  await page.send('Page.navigate', { url: 'about:blank' }); await sleep(1500);
  await browser.send('Browser.close').catch(() => {});
  for (let i = 0; i < 150 && !chromeExited; i++) await sleep(100);
  step('chrome-closed', { exited: chromeExited, jsDialogsAccepted: dialogs });
  if (out.result && !out.result.pass) process.exitCode = 2;
} catch (e) {
  console.log('FAILED', e.message);
  process.exitCode = 1;
} finally {
  if (!chromeExited) { try { chrome.kill(); } catch {} }
}

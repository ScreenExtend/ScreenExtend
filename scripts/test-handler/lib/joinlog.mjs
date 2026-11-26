// A join's stages from the host's verbose log ("[epoch-ms] line", stamped as the line was read).
// `serve` prints no times of its own, so a stage is the first matching line after the join's
// request line; a long silence between two lines is reported as `longestSilenceMs`, because a host
// whose log writes are blocked looks exactly like a host doing nothing.
import fs from 'node:fs';

const STAGES = [
  ['displayLockWait', /display correlation lock: waited (\d+)ms/],
  ['displayCreated', /virtual display created/],
  ['displayAttached', /virtual display id=\d+ attached as/],
  ['modeSettled', /virtual display \S+ (set to|is already at|was created at)/],
  ['pipelineStart', /pipeline: starting live monitor capture/],
  ['adapterSelected', /capture device adapter:/],
  ['encoderCaps', /NVENC caps:|Quick Sync|oneVPL|x264|AMF/],
  ['encoderReady', /encoder initialized|encoder ready|x264 encoder/],
  ['gpuClockHold', /gpu clock hold/],
  ['captureReady', /pipeline: live capture ready/],
  ['answer', /join accepted: WHEP answer generated/],
  ['iceSelected', /selected ICE candidate pair/],
];

// Capture and answer lines, counted rather than first-matched. Every match is kept with its time
// after the join request and its captured groups. Host builds differ in wording, so a request's
// `host_patterns` can replace any of these by key.
export const CAPTURE_PATTERNS = {
  captureTry: /pipeline: starting live monitor capture/,
  staleRetry: /stale before WGC could open it/,
  // "<display> gave no capture item within N ms (K tries)"; the tries are counted by the host
  itemTries: /capture item within (\d+) ms \((\d+) tries\)|\((\d+) tries\)/,
  notCapturable: /is not capturable yet/,
  itemOk: /handed out a capture item after (\d+) ms \((\d+) tries\)/,
  workArea: /work.?area|SPI_SETWORKAREA/i,
  captureOpened: /wgc: session settings in effect|pipeline: live capture ready/,
  firstFrame: /capture: first frame of .* from (\S+) (\d+) ms after it opened/,
  firstFrameReopen: /delivered no frame within (\d+) ms; opening it again from WGC \(reopen (\d+)/,
  answeredAsIs: /reopen\(s\) included; answering with it as it is|answering without one/,
  reopen: /opening it again from WGC|capture (reopen|rebuil)|rebuil(d|t|ding) (the )?(capture|pipeline)|falling back to DXGI|dark rebuild/i,
  scaleWrite: /: scale (\d+)% written; in effect (\S+) before the write, (\S+) at the first read after it,? ?(.*)$/,
  answerReady: /WHEP answer generated \((\d+) bytes\)(?: at wall_ms=(\d+))?/,
  responseReturned: /join response returned at wall_ms=(\d+) \((\d+) ms after the answer was ready, (\d+) ms after the request\)/,
  handlerResumed: /the handler resumed (\d+) ms after the join finished/,
  stillBuilding: /its first attempt is still building; waiting for it/,
  refusedSecond: /still building after .*refusing this one/,
  superseded: /superseded/,
  closedWithoutClient: /closed without a client/,
  displayRemoved: /virtual display .*(removed|removing)|removed virtual display|display id=\d+ .*removed/i,
  telemetryReopens: /capture_first_frame_reopens=(\d+)/,
  // answer before capture: the answer goes out with the capture still pending
  firstFramePending: /first_frame=pending/,
  openedAfterAnswer: /opened (\d+) ms after the answer \(broadcast (-?\d+) ms\)/,
  beforeStart: /before the start: dwm (\d+) ms, item (\d+) ms \(last ask: resolve (\d+) ms, monitor (\d+) ms, CreateForMonitor (\d+) ms\)/,
  blockingPool: /blocking.?pool/i,
  openingTryFailed: /opening try (\d+)\/3 failed:? ?(.*)$/,
  beforeDuplication: /asking for it before duplication starts/,
  sessionEnded: /session for display id=\d+ .*ended|tearing down session/,
  neverReady: /capture_never_ready(?:=(\d+))?/,
  telemetryDark: /capture_dark_rebuilds=(\d+)/,
};
export function setCapturePatterns(over) {
  for (const [k, v] of Object.entries(over || {})) if (k in CAPTURE_PATTERNS) CAPTURE_PATTERNS[k] = new RegExp(v, 'i');
}

export function readLog(file) {
  let s = ''; try { s = fs.readFileSync(file, 'utf8'); } catch { return []; }
  const out = [];
  for (const l of s.split('\n')) { const m = /^\[(\d+)\] (.*)$/.exec(l.replace(/\r$/, '')); if (m) out.push({ t: Number(m[1]), text: m[2] }); }
  return out;
}

/** Every join request in the log for device `name`, each with its stages relative to the request. */
export function joinStages(lines, name) {
  const out = [];
  const idx = lines.map((l, i) => (l.text.startsWith(`join request: device="${name}"`) ? i : -1)).filter((i) => i >= 0);
  for (let k = 0; k < idx.length; k++) {
    // a join ends at the next join request of any device: later joins are not this one's capture
    const i0 = idx[k]; let end = k + 1 < idx.length ? idx[k + 1] : lines.length;
    for (let i = i0 + 1; i < end; i++) if (lines[i].text.startsWith('join request:')) { end = i; break; }
    const t0 = lines[i0].t; const st = { requestAt: t0 };
    let lastT = t0, silence = 0, silenceAfter = null, answerIdx = null;
    for (let i = i0 + 1; i < end; i++) {
      const l = lines[i];
      for (const [key, re] of STAGES) {
        if (st[key] !== undefined) continue;
        const m = re.exec(l.text);
        if (m) st[key] = key === 'displayLockWait' ? Number(m[1]) : l.t - t0;
      }
      if (answerIdx === null) { if (l.t - lastT > silence) { silence = l.t - lastT; silenceAfter = lines[i - 1].text.slice(0, 120); } lastT = l.t; }
      if (st.answer !== undefined && answerIdx === null) answerIdx = i;
      const js = /join stages \((\w+)\): display=(\d+)ms pipeline=(\d+)ms handshake=(\d+)ms/.exec(l.text);
      if (js && !st.hostReported) st.hostReported = { locality: js[1], displayMs: +js[2], pipelineMs: +js[3], handshakeMs: +js[4] };
      if (/join retried attempt|renegotiating in place/.test(l.text)) st.continued = true;
      const dt = l.t - t0;
      for (const [k, re] of Object.entries(CAPTURE_PATTERNS)) {
        const m = re.exec(l.text);
        if (m) (st.ev ||= {})[k] = [...(st.ev[k] || []), [dt, ...m.slice(1).filter((x) => x !== undefined)]].slice(-40);
      }
      const ff = /join stages \(\w+\):.* first_frame=(\S+)/.exec(l.text);
      if (ff && !st.firstFrameStage) st.firstFrameStage = ff[1];
    }
    st.longestSilenceMs = silence; st.silenceAfter = silenceAfter;
    const ev = st.ev || {}; const n = (k) => (ev[k] || []).length; const first = (k) => (ev[k] ? ev[k][0] : null);
    st.captureTries = n('captureTry'); st.staleRetries = n('staleRetry');
    st.itemTries = ev.itemTries ? Math.max(...ev.itemTries.map((x) => Number(x[x.length - 1]) || 0)) : null;
    st.captureOpenedMs = first('captureOpened') ? first('captureOpened')[0] : null;
    st.hostFirstFrameMs = first('firstFrame') ? first('firstFrame')[0] : null;
    st.firstFrameBackend = first('firstFrame') ? first('firstFrame')[1] : st.firstFrameStage || null;
    st.firstFrameReopens = n('firstFrameReopen'); st.answeredAsIs = n('answeredAsIs') > 0;
    st.telemetryReopens = ev.telemetryReopens ? Math.max(...ev.telemetryReopens.map((x) => +x[1])) : 0;
    st.telemetryDark = ev.telemetryDark ? Math.max(...ev.telemetryDark.map((x) => +x[1])) : 0;
    st.reopens = (ev.reopen || []).map((x) => x[0]);
    st.scale = first('scaleWrite') ? { atMs: first('scaleWrite')[0], written: +first('scaleWrite')[1], before: first('scaleWrite')[2], firstRead: first('scaleWrite')[3], rest: first('scaleWrite')[4] } : null;
    const rr = first('responseReturned');
    st.answerReadyMs = first('answerReady') ? first('answerReady')[0] : null;
    st.answerToResponseMs = rr ? +rr[2] : null; st.requestToResponseMs = rr ? +rr[3] : null;
    st.handlerResumedMs = first('handlerResumed') ? +first('handlerResumed')[1] : null;
    out.push(st);
  }
  return out;
}

const r1 = (x) => (x == null ? null : Math.round(x * 10) / 10);
export function distOf(vals) {
  const a = vals.filter((x) => typeof x === 'number' && isFinite(x)).sort((x, y) => x - y);
  if (!a.length) return { n: 0 };
  const q = (p) => a[Math.min(a.length - 1, Math.floor(p * a.length))];
  return { n: a.length, min: r1(a[0]), p50: r1(q(0.5)), p90: r1(q(0.9)), max: r1(a[a.length - 1]), mean: r1(a.reduce((x, y) => x + y, 0) / a.length) };
}
export const STAGE_KEYS = STAGES.map(([k]) => k);

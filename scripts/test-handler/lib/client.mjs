// One test browser: a fresh headless Chrome, one page, the observer script, the paint hook in
// every worker (lib/paint-hook.mjs) that the primary metric reads, and the join.
import { launchChrome, newPage, closeChrome, sleep } from './cdp.mjs';
import { observerSource, GEOMETRY_EXPR, SECONDARY_EXPR, AUDIO_DECODE_TAP_SOURCE } from './inject.mjs';
import { decodePng, pngSize, rgb } from './png.mjs';
import { stampLayout, decodeStamp, unwrap } from './stamp.mjs';
import { dist } from './metrics.mjs';
import { HOOK_SOURCE, VIDEO_SAMPLER_SOURCE } from './paint-hook.mjs';
import { preciseNow } from './clock.mjs';

export class Client {
  constructor({ port, profile, flags = [], headed = false, log, scWidth = 480 }) { Object.assign(this, { port, profile, flags, headed, log, scWidth }); this.frameHandler = null; this.geom = null; this.console = []; this.workers = new Map(); this.stampCfg = null; }

  async open({ client, observer }) {
    this.chrome = await launchChrome({ port: this.port, profile: this.profile, flags: [...this.flags, `--window-size=${client.width},${client.height}`], headed: this.headed });
    this.page = await newPage(this.port);
    const p = this.page;
    p.on((d) => {
      if (d.method === 'Page.screencastFrame') {
        p.send('Page.screencastFrameAck', { sessionId: d.params.sessionId }).catch(() => {});
        if (this.frameHandler) { try { this.frameHandler({ ts: d.params.metadata.timestamp * 1000, data: d.params.data, meta: d.params.metadata, arrived: Date.now() }); } catch (e) { this.log('frame handler: ' + e.message); } }
      } else if (d.method === 'Runtime.consoleAPICalled') {
        if (this.console.length < 3000) this.console.push({ t: Date.now(), level: d.params.type, text: d.params.args.map((a) => (a.value !== undefined ? String(a.value) : a.description || '')).join(' ').slice(0, 400) });
      } else if (d.method === 'Runtime.exceptionThrown') {
        this.console.push({ t: Date.now(), level: 'exception', text: JSON.stringify(d.params.exceptionDetails.exception ? d.params.exceptionDetails.exception.description : d.params.exceptionDetails.text).slice(0, 400) });
      } else if (d.method === 'Log.entryAdded') {
        this.console.push({ t: Date.now(), level: 'log-' + d.params.entry.level, text: (d.params.entry.text + ' ' + (d.params.entry.url || '')).slice(0, 400) });
      }
    });
    // /whep requests from the browser's network log: build-independent join timing.
    // The page's other API requests ride beside them (`apiRequests` in a join): a join that never
    // posts its offer is usually waiting on one of these.
    this.whep = []; this.api = []; const reqs = new Map();
    p.on((d) => {
      const q = d.params || {};
      if (d.method === 'Network.requestWillBeSent' && /\/whep/.test(q.request.url)) { const r = { id: q.requestId, method: q.request.method, url: q.request.url.replace(/^https?:\/\/[^/]+/, ''), sent: q.wallTime * 1000, ts0: q.timestamp }; reqs.set(q.requestId, r); this.whep.push(r); }
      else if (d.method === 'Network.requestWillBeSent' && /^https?:\/\/[^/]+\/(ice-config|net-config|reconfig|leave|audio-outputs|transform-worker\.js|audio-worklet\.js|audio-worker\.js)/.test(q.request.url)) { const r = { id: q.requestId, method: q.request.method, url: q.request.url.replace(/^https?:\/\/[^/]+/, '').replace(/join=[0-9a-f]+/, 'join=<id>'), sent: q.wallTime * 1000, ts0: q.timestamp }; reqs.set(q.requestId, r); this.api.push(r); if (this.api.length > 400) this.api.splice(0, 200); }
      else if (d.method === 'Network.responseReceived' && reqs.has(q.requestId)) {
        const r = reqs.get(q.requestId); r.status = q.response.status; r.answered = r.sent + (q.timestamp - r.ts0) * 1000;
        // Where the time went, on the browser's side: queued before a socket, connecting, TLS,
        // sending, then waiting for the first response byte (ms relative to requestTime).
        const t = q.response.timing;
        if (t) { const k = (x) => (x >= 0 ? Math.round(x) : null); r.timing = { requestTimeOffsetMs: Math.round((t.requestTime - r.ts0) * 1000), connectStart: k(t.connectStart), sslEnd: k(t.sslEnd), sendStart: k(t.sendStart), sendEnd: k(t.sendEnd), receiveHeadersStart: k(t.receiveHeadersStart), receiveHeadersEnd: k(t.receiveHeadersEnd) }; }
        r.connectionId = q.response.connectionId; r.connectionReused = q.response.connectionReused; r.protocol = q.response.protocol;
      }
      else if (d.method === 'Network.loadingFinished' && reqs.has(q.requestId)) { const r = reqs.get(q.requestId); if (!r.answered) { r.answered = r.sent + (q.timestamp - r.ts0) * 1000; r.answeredFrom = 'loadingFinished'; } }
      else if (d.method === 'Network.loadingFailed' && reqs.has(q.requestId)) { const r = reqs.get(q.requestId); r.failed = q.errorText; }
    });
    // Every worker the page starts gets the paint hook through the debugger's auto-attach.
    p.on((d) => {
      if (d.method === 'Target.attachedToTarget') {
        const sid = d.params.sessionId, info = d.params.targetInfo;
        (async () => {
          // Resume first: an evaluate sent to a worker still paused at start can wait for a
          // context that does not exist yet, holding the worker (and the join) for the CDP
          // timeout. The hook patches a prototype, so installing it a moment after the worker's
          // script starts is enough: the first paint comes long after.
          if (d.params.waitingForDebugger) await p.send('Runtime.runIfWaitingForDebugger', {}, sid).catch(() => {});
          if (info.type === 'worker') {
            this.workers.set(sid, { url: info.url });
            await p.send('Runtime.evaluate', { expression: HOOK_SOURCE + AUDIO_DECODE_TAP_SOURCE }, sid, 5000).catch((e) => this.log('hook install: ' + e.message));
            if (this.stampCfg) await p.send('Runtime.evaluate', { expression: `self.__seStamp && (__seStamp.cfg = ${JSON.stringify(this.stampCfg)})` }, sid, 5000).catch(() => {});
          }
        })();
      } else if (d.method === 'Target.detachedFromTarget') this.workers.delete(d.params.sessionId);
    });
    await p.send('Network.enable');
    await p.send('Runtime.enable'); await p.send('Page.enable'); await p.send('Log.enable');
    await p.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true });
    await this.setClient(client);
    await p.send('Page.addScriptToEvaluateOnNewDocument', { source: observerSource(observer || {}) + HOOK_SOURCE + VIDEO_SAMPLER_SOURCE });
    return this;
  }

  async setClient(c) {
    this.clientSpec = c;
    // The observer script fixed the reported refresh rate at the first spec's; a spec change (the
    // hi-res scenario) must change it too, or a "120 Hz" client tells the host 60.
    if (c.refresh && this.page) await this.page.send('Page.addScriptToEvaluateOnNewDocument', { source: `self.__seRefresh = ${Number(c.refresh)};` }).catch(() => {});
    await this.page.send('Emulation.setDeviceMetricsOverride', { width: c.width, height: c.height, deviceScaleFactor: c.dpr, mobile: false, screenWidth: c.width, screenHeight: c.height });
    // The observer pins `screen.refreshRate` once, at open, so a browser switched to another
    // client spec (the standard hi-res scenario reuses the 60 Hz browser) kept reporting the
    // first spec's rate and the "120 Hz" scenario ran at 60. Re-pin it for this page and for
    // every document loaded after it; the later script runs after the observer's and wins.
    if (c.refresh) {
      const pin = `Object.defineProperty(Screen.prototype, 'refreshRate', { get: () => ${Number(c.refresh)}, configurable: true });`;
      await this.page.send('Page.addScriptToEvaluateOnNewDocument', { source: pin }).catch(() => {});
      await this.page.ev(pin).catch(() => {});
    }
  }

  async close() {
    this.stopScreencast().catch(() => {}); await closeChrome(this.chrome);
    for (let i = 0; i < 10; i++) { try { (await import('node:fs')).rmSync(this.profile, { recursive: true, force: true }); break; } catch { await sleep(500); } }
    return { exited: this.chrome ? this.chrome.exited : true };
  }

  async startScreencast(maxWidth = 1280, maxHeight = 1280) {
    await this.page.send('Page.startScreencast', { format: 'png', maxWidth, maxHeight, everyNthFrame: 1 }).catch((e) => this.log('screencast: ' + e.message));
  }
  async stopScreencast() { if (this.page && !this.page.closed) await this.page.send('Page.stopScreencast').catch(() => {}); }

  /** Turn the paint hook on (a cfg) or off (null) in the page and every worker. */
  async setStampCfg(cfg) {
    if (cfg && cfg.mode === 'stamp' && this.stampBand) cfg = { ...cfg, band: this.stampBand };
    this.stampCfg = cfg; const expr = `self.__seStamp && (__seStamp.cfg = ${JSON.stringify(cfg)}, __seStamp.prev = null, true)`;
    await this.page.ev(expr).catch(() => {});
    for (const sid of this.workers.keys()) await this.page.send('Runtime.evaluate', { expression: expr }, sid).catch(() => {});
  }

  /**
   * Each JavaScript context has its own `performance.timeOrigin`, and Chrome takes it from the
   * wall clock at the context's creation. When Windows runs at its default 15.6 ms timer
   * resolution that wall clock is coarse, so two contexts' `timeOrigin + now()` can disagree by
   * up to ~16 ms: measured after a reboot, the page's offset from the system clock was 0.95 to
   * 13.76 ms run to run, and the paint hook's samples come from the *worker*, whose own origin
   * error is independent of the page's. Subtracting one page-measured offset from worker
   * timestamps left a per-run bias of up to ±16 ms in the stamp metric. So every context gets
   * its own offset (min-RTT, like `clockOffset`), measured once per worker and once per page
   * document, and every sample is moved onto the reference clock (`Client.refOffsetMs`, the
   * main page's offset the handler subtracts everywhere) before anyone sees it.
   */
  async contextOffset(sid) {
    let best = null;
    for (let i = 0; i < 15; i++) {
      const t0 = preciseNow();
      const b = sid ? await this.page.send('Runtime.evaluate', { expression: 'performance.timeOrigin + performance.now()', returnByValue: true }, sid).then((r) => r && r.result ? r.result.value : null, () => null)
        : await this.page.ev('performance.timeOrigin + performance.now()').catch(() => null);
      const t1 = preciseNow();
      if (typeof b !== 'number') continue;
      const s = { rtt: t1 - t0, off: b - (t0 + t1) / 2 };
      if (!best || s.rtt < best.rtt) best = s;
    }
    return best;
  }

  /** Everything the hooks have read since the last drain, oldest first, on the reference clock. */
  async drainStamps() {
    const expr = `self.__seStamp ? { out: __seStamp.out.splice(0), painted: __seStamp.painted, sampled: __seStamp.sampled, skipped: __seStamp.skipped, origin: performance.timeOrigin } : null`;
    const all = []; const counters = { painted: 0, sampled: 0, skipped: 0 };
    this.ctxOffsets = this.ctxOffsets || new Map();
    const ref = Client.refOffsetMs;
    const shiftFor = async (key, sid, origin) => {
      if (ref == null) return 0;
      let c = this.ctxOffsets.get(key);
      if (!c || c.origin !== origin) { const o = await this.contextOffset(sid); c = { origin, off: o ? o.off : ref, rtt: o ? o.rtt : null }; this.ctxOffsets.set(key, c); }
      return ref - c.off;
    };
    const add = async (r, where, key, sid) => {
      if (!r) return;
      const shift = r.out.length ? await shiftFor(key, sid, r.origin) : 0;
      for (const x of r.out) all.push({ ...x, t: x.t + shift, where });
      counters.painted += r.painted; counters.sampled += r.sampled; counters.skipped += r.skipped;
    };
    await add(await this.page.ev(expr).catch(() => null), 'page', 'page', null);
    for (const sid of [...this.workers.keys()]) {
      const r = await this.page.send('Runtime.evaluate', { expression: expr, returnByValue: true }, sid).catch(() => null);
      await add(r && r.result ? r.result.value : null, 'worker', sid, sid);
    }
    all.sort((a, b) => a.t - b.t);
    return { samples: all, counters };
  }

  /** The per-context offsets measured so far (ms, context clock minus system clock). */
  clockContexts() { return [...(this.ctxOffsets || new Map()).entries()].map(([k, v]) => ({ ctx: k === 'page' ? 'page' : 'worker', offMs: Math.round(v.off * 100) / 100, rttMs: v.rtt == null ? null : Math.round(v.rtt * 100) / 100 })); }

  /**
   * Decoded-audio onsets every worker has seen since the last drain (AUDIO_DECODE_TAP_SOURCE),
   * with `decoded` moved onto the precise system clock (each worker's own offset; see
   * contextOffset) and the worker-clock value kept as `decodedRaw`.
   */
  async drainAudioDecodeOnsets() {
    const out = []; let packets = 0;
    this.ctxOffsets = this.ctxOffsets || new Map();
    for (const sid of [...this.workers.keys()]) {
      const r = await this.page.send('Runtime.evaluate', { expression: 'self.__seAudTap ? { onsets: __seAudTap.onsets.splice(0), packets: __seAudTap.packets, origin: performance.timeOrigin } : null', returnByValue: true }, sid).catch(() => null);
      const v = r && r.result ? r.result.value : null; if (!v) continue;
      let c = this.ctxOffsets.get(sid);
      if (v.onsets.length && (!c || c.origin !== v.origin)) { const o = await this.contextOffset(sid); c = { origin: v.origin, off: o ? o.off : 0, rtt: o ? o.rtt : null }; this.ctxOffsets.set(sid, c); }
      for (const x of v.onsets) out.push({ ...x, decodedRaw: x.decoded, decoded: x.decoded - (c ? c.off : 0) });
      packets += v.packets;
    }
    return { onsets: out.sort((a, b) => a.decoded - b.decoded), packets };
  }

  async geometry() { try { this.geom = await this.page.ev(GEOMETRY_EXPR); } catch { /* navigating */ } return this.geom; }
  async secondary() { try { return await this.page.ev(SECONDARY_EXPR, { timeout: 8000 }); } catch (e) { return { error: e.message }; } }

  /** Close dialogs the page raises before and during a join. `micChoice`: 'continue' | 'skip'. */
  async sweepModals(micChoice = 'continue') {
    return this.page.ev(`(() => {
      const shown = (id) => { const m = document.getElementById(id); if (!m) return false; const s = getComputedStyle(m); return s.display !== 'none' && s.visibility !== 'hidden' && (m.classList.contains('show') || m.style.display === 'flex' || m.style.display === 'block'); };
      if (shown('sameDeviceModal')) { document.getElementById('sameDeviceDismiss').click(); return 'sameDevice'; }
      if (shown('compatModal')) { const b = document.getElementById('compatContinue'); if (b) { b.click(); return 'compat'; } return 'compat(blocking)'; }
      if (shown('permModal')) { const b = document.getElementById(${JSON.stringify(micChoice === 'skip' ? 'permSkip' : 'permContinue')}) || document.getElementById('permContinue'); b.click(); return 'micCard:' + b.id; }
      const h = document.getElementById('dismiss'); if (h && h.offsetParent) { h.click(); return 'httpsUpsell'; }
      return null; })()`, { gesture: true }).catch(() => null);
  }

  /**
   * Navigate and join. Returns timing measured on the browser's clock:
   * submit -> /whep answer (the browser's network log) -> first painted frame, i.e. the first
   * screencast frame carrying a readable clock stamp from `stage` (a stage.ps1 started before
   * the submit, waiting for the join's display to attach and drawing the moment it does).
   */
  async join({ url, origin, grant, micChoice = 'continue', timeoutMs = 40000, stage = null, restage = null }) {
    const p = this.page; const res = { modals: [], url: url.replace(/otp=\d+/, 'otp=<otp>') };
    let firstPicture = null; let submitAt = null; let stageInfo = null;
    for (let attempt = 0; attempt < 3; attempt++) {
      res.navigate = await p.send('Page.navigate', { url }).then((r) => (r.errorText ? r.errorText : 'ok'), (e) => e.message);
      if (res.navigate === 'ok') break;
      await sleep(1500);
    }
    for (let i = 0; i < 75; i++) { if (await p.ev('document.readyState === "complete" && !!document.getElementById("joinForm")').catch(() => false)) break; await sleep(200); }
    res.page = await p.ev('({ secure: isSecureContext, coi: typeof crossOriginIsolated !== "undefined" && crossOriginIsolated, title: document.title })').catch((e) => ({ error: e.message }));
    if (grant !== null && grant !== undefined) {
      await this.chrome.browser.send('Browser.grantPermissions', { origin, permissions: grant }).catch((e) => { res.grantError = e.message; });
    }
    for (let i = 0; i < 10; i++) { const m = await this.sweepModals(micChoice); if (m) res.modals.push(m); await sleep(150); }
    const canJoin = await p.ev('!document.getElementById("connect").disabled').catch(() => false);
    if (!canJoin) {
      const diag = await p.ev('({ href: location.href.replace(/otp=\d+/, "otp=<otp>"), ready: document.readyState, err: (document.getElementById("errorMessage") || {}).textContent, title: document.title })').catch((e) => ({ evalError: e.message }));
      res.error = 'Connect stayed disabled: ' + JSON.stringify(diag); this.frameHandler = null; await this.stopScreencast(); return res;
    }
    // Started now, not earlier, so its wait for the display covers the join and nothing else.
    const st = stage ? stage() : null;
    if (st) { this.joinStage = st; st.ready.then((r) => { stageInfo = r; res.stage = r; if (!r.error) this.setStampCfg({ mode: 'stamp', width: r.width, height: r.height, cell: r.cell, cellH: r.cellH }); }); }
    submitAt = await p.ev('(() => { const t = performance.timeOrigin + performance.now(); document.getElementById("joinForm").requestSubmit(); return t; })()', { gesture: true });
    const t0 = Date.now();
    const answered = () => this.whep.some((w) => w.method === 'POST' && w.sent >= submitAt - 50 && (w.answered || w.failed));
    while (Date.now() - t0 < timeoutMs && !(firstPicture && answered())) {
      const m = await this.sweepModals(micChoice); if (m) res.modals.push(m);
      // The page can retry the offer (a slow answer, a fallback); the stream it ends on may be
      // on a different display from the one the stage found. Follow it.
      const lastAnswer = Math.max(0, ...this.whep.filter((w) => w.method === 'POST' && w.answered && w.sent >= submitAt - 50).map((w) => w.answered));
      // Frames that paint without a stamp ('guard') mean the stream is up but the stage is not on
      // it, or drew at a stale geometry: the display often attaches at the first mode of the DPR
      // ladder (e.g. 2400x1350) and is set to the client's size a moment later, and the stage
      // kept the first geometry. That cost 4-6 s of every slow "first picture" on the current
      // head (hookBeforeFirstPicture: 180-295 guard reads before the restage). Restage after
      // 0.8 s of guard-only paints instead of waiting 4 s.
      const hk = res.hookBeforeFirstPicture || {};
      const guardOnly = (hk.guard || 0) >= 20 && (hk.stamp || 0) * 10 < hk.guard;
      const restageAfter = guardOnly ? 800 : 4000;
      if (restage && !firstPicture && lastAnswer && Date.now() > lastAnswer + restageAfter && (!res.restagedFor || res.restagedFor < lastAnswer)) {
        res.restagedFor = lastAnswer; stageInfo = null;
        const st2 = await restage(this.joinStage);
        if (st2) { this.joinStage = st2; st2.ready.then((r) => { stageInfo = r; res.stage2 = r; if (!r.error) this.setStampCfg({ mode: 'stamp', width: r.width, height: r.height, cell: r.cell, cellH: r.cellH }); }); }
      }
      // The display may still be changing mode after the attach; the stage reports every change.
      const cur = this.joinStage && this.joinStage.current;
      if (stageInfo && cur && !cur.error && (cur.width !== stageInfo.width || cur.cell !== stageInfo.cell)) { stageInfo = cur; res.stageUpdates = (res.stageUpdates || 0) + 1; await this.setStampCfg({ mode: 'stamp', width: cur.width, height: cur.height, cell: cur.cell, cellH: cur.cellH }); }
      if (stageInfo && !firstPicture) {
        const { samples } = await this.drainStamps();
        res.hookBeforeFirstPicture = res.hookBeforeFirstPicture || {};
        for (const x of samples) { const k = x.v !== undefined ? 'stamp' : x.e; res.hookBeforeFirstPicture[k] = (res.hookBeforeFirstPicture[k] || 0) + 1; }
        const hit = samples.find((s) => s.v !== undefined && s.t >= submitAt);
        if (hit) { firstPicture = hit.t; res.firstStampAgeMs = Math.round(hit.t - (Client.refOffsetMs || 0) - unwrap(hit.v, hit.t)); res.firstPaintVia = hit.where; }
      }
      await sleep(50);
    }
    await this.setStampCfg(null); await this.geometry();
    const firstPost = this.whep.find((w) => w.method === 'POST' && w.sent >= submitAt - 50);
    res.submitAt = submitAt;
    res.answerMs = firstPost && firstPost.answered ? Math.round(firstPost.answered - submitAt) : null;
    res.answerStatus = firstPost ? firstPost.status : null;
    res.whepRequests = this.whep.filter((w) => w.sent >= submitAt - 50).map(({ ts0, id, ...w }) => ({ ...w, sentMs: Math.round(w.sent - submitAt), answeredMs: w.answered ? Math.round(w.answered - submitAt) : null, sent: undefined, answered: undefined }));
    res.apiRequests = this.api.filter((w) => w.sent >= submitAt - 50).slice(0, 60).map(({ ts0, id, timing, connectionId, connectionReused, ...w }) => ({ ...w, sentMs: Math.round(w.sent - submitAt), answeredMs: w.answered ? Math.round(w.answered - submitAt) : null, sent: undefined, answered: undefined }));
    res.firstPictureMs = firstPicture ? Math.round(firstPicture - submitAt) : null;
    res.offers = res.whepRequests.filter((w) => w.method === 'POST').length;
    if (res.offers > 1) res.retried = 'the page sent ' + res.offers + ' offers (a slow or failed answer made it retry)';
    res.pageError = await p.ev('(document.getElementById("errorMessage") || {}).textContent || ""').catch(() => '');
    res.geometry = this.geom;
    return res;
  }

  /**
   * Read stamps from every screencast frame for `ms`. Returns raw samples
   * [{ ts, stamp (epoch ms, system clock) | error }].
   */
  async collectStamps(stage, ms, { method = 'paint', follow = null } = {}) {
    if (method === 'screencast') return this.collectStampsScreencast(stage, ms);
    await this.drainStamps();
    let cur = stage; let geometryChanges = 0;
    await this.setStampCfg({ mode: 'stamp', width: cur.width, height: cur.height, cell: cur.cell, cellH: cur.cellH });
    const samples = []; let counters = null; const t0 = Date.now();
    while (Date.now() - t0 < ms) {
      await sleep(250); const d = await this.drainStamps(); samples.push(...d.samples); counters = d.counters;
      // A settings edit can change the display's size under the stage; the stage follows it and
      // says so, and the reader follows the stage.
      const n = follow && follow.current;
      if (n && !n.error && (n.width !== cur.width || n.height !== cur.height || n.cell !== cur.cell)) { cur = n; geometryChanges++; await this.setStampCfg({ mode: 'stamp', width: cur.width, height: cur.height, cell: cur.cell, cellH: cur.cellH }); }
    }
    await this.setStampCfg(null); await sleep(100);
    const d = await this.drainStamps(); samples.push(...d.samples); counters = d.counters;
    return { samples: samples.map((s) => (s.v !== undefined ? { ts: s.t, stamp: unwrap(s.v, s.t), where: s.where } : { ts: s.t, error: s.e, where: s.where, luma: s.l })), counters, geometryChanges, method: 'paint hook' };
  }

  async collectStampsScreencast(stage, ms, { maxWidth = this.scWidth } = {}) {
    const samples = []; let L = null; let lastGeomAt = 0;
    this.frameHandler = (f) => {
      const g = this.geom; if (!g || !g.live) { samples.push({ ts: f.ts, error: 'not live' }); return; }
      const buf = Buffer.from(f.data, 'base64');
      L = stampLayout(g, stage, pngSize(buf));
      if (!L) { samples.push({ ts: f.ts, error: 'no layout' }); return; }
      const img = decodePng(buf, L.maxRow);
      const d = decodeStamp(img, L);
      samples.push(d.value !== undefined ? { ts: f.ts, stamp: unwrap(d.value, f.ts) } : { ts: f.ts, error: d.error });
    };
    await this.geometry();
    await this.startScreencast(maxWidth, maxWidth);
    const t0 = Date.now();
    while (Date.now() - t0 < ms) { await sleep(250); if (Date.now() - lastGeomAt > 1000) { await this.geometry(); lastGeomAt = Date.now(); } }
    await this.stopScreencast();
    this.frameHandler = null;
    return { samples, layout: L };
  }

  /** Static-desktop pixel stability: a grid of samples per frame, compared frame to frame. */
  async collectStatic(ms) {
    await this.drainStamps();
    await this.setStampCfg({ mode: 'grid' });
    const out = []; const t0 = Date.now();
    while (Date.now() - t0 < ms) { await sleep(500); out.push(...(await this.drainStamps()).samples); }
    await this.setStampCfg(null); await sleep(100); out.push(...(await this.drainStamps()).samples);
    const frames = out.filter((x) => x.changed !== undefined);
    const changes = frames.filter((x, i) => i > 0 && x.changed > 0);
    return { paintedFramesSampled: frames.length, paintedFps: Math.round((frames.length / (ms / 1000)) * 10) / 10, framesWithChange: changes.length, firstChanges: changes.slice(0, 20).map((c) => ({ t: Math.round(c.t), samplesChanged: c.changed, maxDelta: c.maxDelta, green: c.green })), greenFrames: frames.filter((x) => x.green > 0).length, maxGreenSamples: Math.max(0, ...frames.map((x) => x.green || 0)), gridSamples: 64 * 36, method: 'paint hook: every painted frame downscaled to 64x36 and compared with the one before', changeThreshold: 'any RGB channel moved by more than 24 at a grid point' };
  }

  async collectStaticScreencast(ms) {
    const sigs = []; let ref = null; const changes = []; let greenFrames = 0, maxGreen = 0, frames = 0;
    this.frameHandler = (f) => {
      const g = this.geom; if (!g || !g.live) return;
      const img = decodePng(Buffer.from(f.data, 'base64')); const k = img.width / g.vw;
      const sig = [];
      for (let yi = 0; yi < 36; yi++) for (let xi = 0; xi < 64; xi++) {
        const [r, gg, b] = rgb(img, Math.min(img.width - 1, (g.x + g.w * (xi + 0.5) / 64) * k), Math.min(img.rows - 1, (g.y + g.h * (yi + 0.5) / 36) * k));
        sig.push(r, gg, b);
      }
      frames++;
      if (ref) {
        let diff = 0, maxd = 0, green = 0;
        for (let i = 0; i < sig.length; i += 3) {
          const d = Math.max(Math.abs(sig[i] - ref[i]), Math.abs(sig[i + 1] - ref[i + 1]), Math.abs(sig[i + 2] - ref[i + 2]));
          if (d > 24) diff++;
          if (d > maxd) maxd = d;
          if (sig[i + 1] > sig[i] + 40 && sig[i + 1] > sig[i + 2] + 40 && !(ref[i + 1] > ref[i] + 40 && ref[i + 1] > ref[i + 2] + 40)) green++;
        }
        if (diff) changes.push({ ts: f.ts, samplesChanged: diff, maxDelta: maxd });
        if (green) { greenFrames++; maxGreen = Math.max(maxGreen, green); }
      }
      ref = sig; sigs.push(f.ts);
    };
    await this.geometry();
    await this.startScreencast(640, 640);
    const t0 = Date.now(); while (Date.now() - t0 < ms) { await sleep(500); await this.geometry(); }
    await this.stopScreencast();
    this.frameHandler = null;
    return { screencastFrames: frames, framesWithChange: changes.length, firstChanges: changes.slice(0, 20), greenFrames, maxGreenSamples: maxGreen, gridSamples: 64 * 36, changeThreshold: 'any RGB channel moved by more than 24 at a sampled point' };
  }
}

/**
 * Turn raw stamp samples into the primary metric. `offsetMs` is (browser clock - system clock),
 * subtracted so a stamp drawn on the system clock compares with a screencast time on the
 * browser's clock.
 */
export function stampMetrics(samples, { from, to, offsetMs = 0 }) {
  const inWin = samples.filter((s) => s.ts >= from && s.ts <= to);
  const errors = {}; for (const s of inWin) if (s.error) errors[s.error] = (errors[s.error] || 0) + 1;
  // The cell luminances of the first failed reads (paint hook keeps up to 40 per context), so a
  // failure can be told apart: two valid stamps torn together, or cells blurred towards grey.
  const errorLuma = inWin.filter((s) => s.luma).slice(0, 12).map((s) => ({ ts: Math.round(s.ts), luma: s.luma }));
  // A misread that slips past the 8-bit check shows up as an impossible age; count it, drop it.
  const ok = inWin.filter((s) => s.stamp !== undefined && s.ts - offsetMs - s.stamp > -5 && s.ts - offsetMs - s.stamp < 5000);
  const implausible = inWin.filter((s) => s.stamp !== undefined).length - ok.length;
  if (implausible) errors.implausible = implausible;
  const lat = []; const newTs = []; const incr = []; let prev = null, dups = 0, backwards = 0;
  for (const s of ok) {
    if (prev && s.stamp === prev.stamp) { dups++; continue; }
    if (prev && s.stamp < prev.stamp) { backwards++; }
    lat.push(s.ts - offsetMs - s.stamp); newTs.push(s.ts);
    if (prev) incr.push(s.stamp - prev.stamp);
    prev = s;
  }
  const gaps = []; for (let i = 1; i < newTs.length; i++) gaps.push(newTs[i] - newTs[i - 1]);
  const secs = (to - from) / 1000;
  const sortedIncr = [...incr].sort((a, b) => a - b); const medIncr = sortedIncr.length ? sortedIncr[sortedIncr.length >> 1] : null;
  return {
    latencyMs: dist(lat),
    samples: inWin.length, decoded: ok.length, decodeErrors: errors, errorLuma,
    newStamps: lat.length, paintedFps: secs > 0 ? Math.round((lat.length / secs) * 10) / 10 : null,
    duplicateFrames: dups, backwardsStamps: backwards,
    stampIncrementMs: dist(incr), skippedStamps: medIncr ? incr.filter((x) => x > 1.5 * medIncr).length : null,
    longestGapMs: gaps.length ? Math.round(Math.max(...gaps)) : null, gapsOver100ms: gaps.filter((g) => g > 100).length,
    windowSeconds: Math.round(secs * 10) / 10,
  };
}

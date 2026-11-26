'use strict';
(function () {
  const OP = {
    POINTER_DOWN: 0x01, POINTER_UP: 0x02, POINTER_MOVE: 0x03, POINTER_CANCEL: 0x04,
    POINTER_ENTER: 0x05, POINTER_LEAVE: 0x06, POINTER_OVER: 0x07, POINTER_OUT: 0x08,
    POINTER_MOVE_BATCH: 0x09,
    WHEEL: 0x10, ZOOM: 0x11, KEY: 0x20, TEXT_INPUT: 0x21, COMPOSITION_UPDATE: 0x22,
    CLIPBOARD: 0x30, DRAG: 0x40, DROP: 0x41,
    FOCUS_STATE: 0x50, VISIBILITY: 0x51, RESIZE: 0x52, POINTERLOCK_STATE: 0x53,
    MOUSE_DELTA: 0x54, PING: 0x60, PONG: 0x61, STATS: 0x62, BYE: 0x63,
    KEYFRAME_REQUEST: 0x64,
  };

  // BigInt is Safari 14 / Chrome 67 and this file loads on clients below that, so a u64 is two
  // little-endian u32 halves (byte-identical to setBigUint64). Every value sent is under 2^53,
  // so the Number round trip is exact.
  const U32 = 4294967296;
  function setU64(view, off, value) {
    const v = value < 0 ? 0 : value;
    const hi = Math.floor(v / U32);
    view.setUint32(off, (v - hi * U32) >>> 0, true);
    view.setUint32(off + 4, hi >>> 0, true);
  }
  function getU64(view, off) {
    return view.getUint32(off, true) + view.getUint32(off + 4, true) * U32;
  }
  const SRC = { mouse: 0x00, touch: 0x01, pen: 0x02 };
  // lazy: this file is one IIFE, so a throw at module scope would take RemoteInput and SEClock
  // (remote control, clock sync, telemetry) down silently
  let textEncoder = null;
  function enc(s) {
    if (!textEncoder) textEncoder = new TextEncoder();
    return textEncoder.encode(s);
  }
  // performance.timeOrigin is Safari 15; below that, compute it the long way or the clock
  // offset is NaN
  const TIME_ORIGIN = (typeof performance.timeOrigin === 'number' && isFinite(performance.timeOrigin))
    ? performance.timeOrigin
    : (Date.now() - performance.now());
  const settings = { sensitivity: 1.0, accel: 0.0 };
  try {
    const saved = JSON.parse(localStorage.getItem('rib.settings') || '{}');
    if (typeof saved.sensitivity === 'number') settings.sensitivity = saved.sensitivity;
    if (typeof saved.accel === 'number') settings.accel = saved.accel;
  } catch (_) {}
  function applyMouseCurve(dx, dy) {
    let gx = settings.sensitivity, gy = settings.sensitivity;
    if (settings.accel > 0) {
      const boost = 1 + settings.accel * (Math.hypot(dx, dy) / 16);
      gx *= boost; gy *= boost;
    }
    return [dx * gx, dy * gy];
  }

  let fast = null, reliable = null, bulk = null;
  let pingTimer = null;
  let installed = false;
  let active = false;
  // set while the page has a dialog or the browser's output picker in front of the user
  let suspended = false;
  let surface = null;
  let imeSink = null;

  let cssW = window.innerWidth, cssH = window.innerHeight;
  // the picture is laid out `object-fit: contain`, so a stage with a different aspect ratio
  // has bars: normalise against the rendered picture's rect, not the stage
  let picX = 0, picY = 0, picW = 0, picH = 0;
  let srcW = 0, srcH = 0;
  function refreshSize() {
    cssW = (surface && surface.clientWidth) || window.innerWidth;
    cssH = (surface && surface.clientHeight) || window.innerHeight;
    recomputePictureRect();
  }
  function recomputePictureRect() {
    if (!(srcW > 0 && srcH > 0 && cssW > 0 && cssH > 0)) {
      picX = 0; picY = 0; picW = cssW; picH = cssH;
      return;
    }
    const scale = Math.min(cssW / srcW, cssH / srcH);
    picW = srcW * scale;
    picH = srcH * scale;
    picX = (cssW - picW) / 2;
    picY = (cssH - picH) / 2;
  }
  function setPictureSize(w, h) {
    if (!(w > 0 && h > 0)) return;
    if (w === srcW && h === srcH) return;
    srcW = w; srcH = h;
    recomputePictureRect();
  }

  // each buffer is sized for its optional tail and sent with an explicit length. v1 lengths:
  // pointer 40, wheel 15, mouse delta 8 (byte 8 is a pad in v1), batch 10 + n*24
  const POINTER_LEN = 40, WHEEL_LEN = 15, DELTA_LEN = 9, STAMP_LEN = 8, SEQ_LEN = 2;
  const moveBuf = new ArrayBuffer(POINTER_LEN + STAMP_LEN + SEQ_LEN), moveView = new DataView(moveBuf);
  const wheelBuf = new ArrayBuffer(WHEEL_LEN + STAMP_LEN + SEQ_LEN), wheelView = new DataView(wheelBuf);
  const deltaBuf = new ArrayBuffer(DELTA_LEN + STAMP_LEN + SEQ_LEN), deltaView = new DataView(deltaBuf);
  const resizeBuf = new ArrayBuffer(9), resizeView = new DataView(resizeBuf);
  const lifeBuf = new ArrayBuffer(2), lifeView = new DataView(lifeBuf);
  const pingBuf = new ArrayBuffer(9), pingView = new DataView(pingBuf);
  const zoomBuf = new ArrayBuffer(5), zoomView = new DataView(zoomBuf);
  const keyScratch = new Uint8Array(1024), keyView = new DataView(keyScratch.buffer);
  const BATCH_MAX = 40;
  const BATCH_TAIL = STAMP_LEN + 2;
  const batchBuf = new ArrayBuffer(10 + BATCH_MAX * 24 + BATCH_TAIL + SEQ_LEN), batchView = new DataView(batchBuf);

  function clamp01(v) { return v < 0 ? 0 : v > 1 ? 1 : v; }
  function clampI16(v) { return v < -32768 ? -32768 : v > 32767 ? 32767 : (v | 0); }

  // The optional tails. Both are gated on the host having said it reads them: the flags byte
  // is one an older host reads as a zero pad. `hostCaps` is 0 until the first PONG, so a
  // session opens with plain v1 frames.
  // The stamp is `performance.now() * 1e6`, the PING's own value and timebase, so the host can
  // difference the two with no shared epoch.
  const EVFLAG_UNADJUSTED = 1 << 0;
  let lockIsUnadjusted = false;
  function wantStamp() { return (hostCaps & HOST_CAP.INPUT_TIMESTAMP) !== 0; }
  function wantEvFlags() { return (hostCaps & HOST_CAP.INPUT_EVENT_FLAGS) !== 0; }
  function nowNs() { return Math.round(performance.now() * 1e6); }
  function evFlags() { return lockIsUnadjusted ? EVFLAG_UNADJUSTED : 0; }

  // `fast` is unordered with no retransmits and the host picks randomly between its two
  // queues, so a move can land behind the one that superseded it: one counter per page, bumped
  // per frame sent on `fast`, lets the host drop the stale one. It sits past the stamp, found
  // by offset, so it rides only with the stamp. Never reset it: a restart at 0 against a
  // host's per-session high-water mark is rejected for half the sequence space.
  let fastSeq = 0;
  function wantSeq() { return wantStamp() && (hostCaps & HOST_CAP.INPUT_SEQUENCE) !== 0; }
  function nextSeq() { const s = fastSeq; fastSeq = (fastSeq + 1) & 0xffff; return s; }

  function channelFor(op) {
    switch (op) {
      case OP.POINTER_MOVE: case OP.POINTER_MOVE_BATCH: case OP.POINTER_ENTER:
      case OP.POINTER_LEAVE: case OP.POINTER_OVER: case OP.POINTER_OUT:
      case OP.WHEEL: case OP.ZOOM: case OP.MOUSE_DELTA:
        return fast;
      case OP.CLIPBOARD: case OP.DRAG: case OP.DROP:
        return bulk;
      default:
        return reliable;
    }
  }
  const FAST_BUFFER_MAX = 8192;
  let fastDropped = 0;
  let sendFailures = 0;
  function rawSend(ch, buf, len) {
    if (!ch || ch.readyState !== 'open') return;
    try { ch.send(len === undefined ? buf : new Uint8Array(buf, 0, len)); }
    catch (e) {
      noteSuppressed();
      // name a throwing send once per session, or a dead channel looks like nothing to send
      if (sendFailures++ === 0) console.warn('[input] a ' + (ch.label || '?') + ' send threw: ' + e);
    }
  }
  function fastSend(buf, len) {
    if (!fast || fast.readyState !== 'open') return;
    // dropping under backpressure is right here (a stale cursor position is worth nothing),
    // but it is counted
    if (fast.bufferedAmount > FAST_BUFFER_MAX) { fastDropped++; return; }
    try { fast.send(len === undefined ? buf : new Uint8Array(buf, 0, len)); } catch (_) { noteSuppressed(); }
  }
  function send(op, buf, len) {
    const ch = channelFor(op);
    if (ch === fast) fastSend(buf, len); else rawSend(ch, buf, len);
  }

  function modMask(e) {
    let m = 0;
    if (e.shiftKey) m |= 1 << 0;
    if (e.ctrlKey) m |= 1 << 1;
    if (e.altKey) m |= 1 << 2;
    if (e.metaKey) m |= 1 << 3;
    if (e.getModifierState) {
      if (e.getModifierState('AltGraph')) m |= 1 << 4;
      if (e.getModifierState('CapsLock')) m |= 1 << 5;
      if (e.getModifierState('NumLock')) m |= 1 << 6;
    }
    return m;
  }
  function sourceByte(e) {
    return e.pointerType === 'touch' ? SRC.touch : e.pointerType === 'pen' ? SRC.pen : SRC.mouse;
  }
  function normX(e) { return clamp01((e.clientX - picX) / (picW || cssW || 1)); }
  function normY(e) { return clamp01((e.clientY - picY) / (picH || cssH || 1)); }

  function sendPointer(op, e, ch) {
    moveView.setUint8(0, op);
    moveView.setUint8(1, sourceByte(e));
    moveView.setUint32(2, (e.pointerId >>> 0), true);
    moveView.setFloat32(6, normX(e), true);
    moveView.setFloat32(10, normY(e), true);
    moveView.setFloat32(14, e.pressure != null ? e.pressure : 0, true);
    moveView.setFloat32(18, e.tiltX || 0, true);
    moveView.setFloat32(22, e.tiltY || 0, true);
    moveView.setFloat32(26, e.twist || 0, true);
    moveView.setFloat32(30, (e.width || 0) / (picW || cssW || 1), true);
    moveView.setFloat32(34, (e.height || 0) / (picH || cssH || 1), true);
    moveView.setUint16(38, e.buttons || 0, true);
    let len = POINTER_LEN;
    if (wantStamp()) {
      setU64(moveView, POINTER_LEN, nowNs());
      len += STAMP_LEN;
      // fast channel only: a reliable frame is ordered already and the host looks for no
      // sequence on one
      if (wantSeq() && fast && (ch || channelFor(op)) === fast) {
        moveView.setUint16(len, nextSeq(), true);
        len += SEQ_LEN;
      }
    }
    if (ch) rawSend(ch, moveBuf, len); else send(op, moveBuf, len);
  }

  function sendPointerBatch(source, id, buttons, coalesced) {
    for (let start = 0; start < coalesced.length; start += BATCH_MAX) {
      const n = Math.min(BATCH_MAX, coalesced.length - start);
      batchView.setUint8(0, OP.POINTER_MOVE_BATCH);
      batchView.setUint8(1, source);
      batchView.setUint32(2, id >>> 0, true);
      batchView.setUint16(6, buttons || 0, true);
      batchView.setUint16(8, n, true);
      let o = 10;
      for (let i = start; i < start + n; i++) {
        const s = coalesced[i];
        batchView.setFloat32(o, clamp01((s.clientX - picX) / (picW || cssW || 1)), true);
        batchView.setFloat32(o + 4, clamp01((s.clientY - picY) / (picH || cssH || 1)), true);
        batchView.setFloat32(o + 8, s.pressure != null ? s.pressure : 0, true);
        batchView.setFloat32(o + 12, s.tiltX || 0, true);
        batchView.setFloat32(o + 16, s.tiltY || 0, true);
        batchView.setFloat32(o + 20, s.twist || 0, true);
        o += 24;
      }
      // The tail is all-or-nothing: the host reads it only when the stamp, the flags byte and
      // the predicted count are all present. The count is 0 because only measured samples are
      // sent (getPredictedEvents() is deliberately not used). The flags byte is a reserved zero
      // the host does not read: `UNADJUSTED` means pointer lock with raw movement, and a locked
      // pointer sends MOUSE_DELTA, never a batch.
      if (wantStamp()) {
        setU64(batchView, o, nowNs()); o += STAMP_LEN;
        batchView.setUint8(o++, 0);
        batchView.setUint8(o++, 0);
        if (wantSeq()) { batchView.setUint16(o, nextSeq(), true); o += SEQ_LEN; }
      }
      send(OP.POINTER_MOVE_BATCH, batchBuf, o);
    }
  }

  function sendZoom(delta) {
    if (!delta) return;
    zoomView.setUint8(0, OP.ZOOM);
    zoomView.setFloat32(1, delta, true);
    send(OP.ZOOM, zoomBuf);
  }
  function sendWheel(e) {
    wheelView.setUint8(0, OP.WHEEL);
    wheelView.setUint8(1, SRC.mouse);
    wheelView.setFloat32(2, e.deltaX, true);
    wheelView.setFloat32(6, e.deltaY, true);
    wheelView.setFloat32(10, e.deltaZ || 0, true);
    wheelView.setUint8(14, e.deltaMode || 0);
    let len = WHEEL_LEN;
    if (wantStamp()) {
      setU64(wheelView, WHEEL_LEN, nowNs());
      len += STAMP_LEN;
      if (wantSeq()) { wheelView.setUint16(len, nextSeq(), true); len += SEQ_LEN; }
    }
    send(OP.WHEEL, wheelBuf, len);
  }
  function sendMouseDelta(dx, dy, buttons) {
    deltaView.setUint8(0, OP.MOUSE_DELTA);
    deltaView.setUint8(1, 0);
    deltaView.setInt16(2, clampI16(dx), true);
    deltaView.setInt16(4, clampI16(dy), true);
    deltaView.setUint16(6, buttons || 0, true);
    // byte 8 is a zero pad to a host that does not read it. Bit 0: these deltas are raw device
    // counts with no OS pointer curve on them, so the host knows not to apply a second one
    deltaView.setUint8(8, wantEvFlags() ? evFlags() : 0);
    let len = DELTA_LEN;
    if (wantStamp()) {
      setU64(deltaView, DELTA_LEN, nowNs());
      len += STAMP_LEN;
      if (wantSeq()) { deltaView.setUint16(len, nextSeq(), true); len += SEQ_LEN; }
    }
    send(OP.MOUSE_DELTA, deltaBuf, len);
  }

  function sendKey(down, e) {
    const code = enc(e.code || '');
    const key = enc(e.key || '');
    if (code.length > 255 || key.length > 255) return;
    let o = 0;
    keyView.setUint8(o++, OP.KEY);
    keyView.setUint8(o++, (down ? 1 : 0) | (e.repeat ? 2 : 0));
    keyView.setUint16(o, modMask(e), true); o += 2;
    keyView.setUint8(o++, code.length);
    keyScratch.set(code, o); o += code.length;
    keyView.setUint8(o++, key.length);
    keyScratch.set(key, o); o += key.length;
    if (wantStamp()) { setU64(keyView, o, nowNs()); o += STAMP_LEN; }
    send(OP.KEY, keyScratch.buffer, o);
  }

  function sendText(op, str) {
    if (!str) return;
    const data = enc(str);
    const buf = new ArrayBuffer(5 + data.length);
    const v = new DataView(buf);
    v.setUint8(0, op);
    v.setUint32(1, data.length, true);
    new Uint8Array(buf, 5).set(data);
    send(op, buf);
  }

  function sendClipboard(opByte, mime, dataBytes) {
    const m = enc(mime);
    const buf = new ArrayBuffer(6 + m.length + 4 + dataBytes.length);
    const v = new DataView(buf);
    v.setUint8(0, OP.CLIPBOARD);
    v.setUint8(1, opByte);
    v.setUint32(2, m.length, true);
    new Uint8Array(buf, 6).set(m);
    v.setUint32(6 + m.length, dataBytes.length, true);
    new Uint8Array(buf, 10 + m.length).set(dataBytes);
    send(OP.CLIPBOARD, buf);
  }

  function sendDrag(phase, x, y) {
    const buf = new ArrayBuffer(10);
    const v = new DataView(buf);
    v.setUint8(0, OP.DRAG);
    v.setUint8(1, phase);
    v.setFloat32(2, clamp01(x), true);
    v.setFloat32(6, clamp01(y), true);
    send(OP.DRAG, buf);
  }
  // Blob.prototype.arrayBuffer is Chrome 76 / Safari 14 / Firefox 69: below that, read through
  // a FileReader or the drop is lost to an unhandled rejection
  function fileBytes(f) {
    if (typeof f.arrayBuffer === 'function') return f.arrayBuffer();
    return new Promise(function (resolve, reject) {
      const fr = new FileReader();
      fr.onload = function () { resolve(fr.result); };
      fr.onerror = function () { reject(fr.error || new Error('FileReader failed')); };
      fr.readAsArrayBuffer(f);
    });
  }
  // A dropped file is one SCTP message (no chunking opcode, no reassembly on the host), so one
  // above the negotiated maximum would throw in rawSend and vanish. The limit is enforced only
  // where the browser reports one: `maxMessageSize` can be Infinity, and `pc.sctp` is absent
  // below Chrome 76 and on Safari. Checked against `File.size`, before any read.
  let dropPc = null;
  function bulkMessageLimit() {
    try {
      const s = dropPc && dropPc.sctp;
      const m = s && s.maxMessageSize;
      return (typeof m === 'number' && isFinite(m) && m > 0) ? m : 0;
    } catch (_) { return 0; }
  }
  const DROP_HEADER = 12;
  function dropItemOverhead(f) {
    // u16 name len + name + u32 mime len + mime + u64 size + u64 data len
    return 2 + enc(f.name || '').length + 4 + enc(f.type || 'application/octet-stream').length + 16;
  }
  let dropsRefused = 0;
  function screenDrop(files) {
    const limit = bulkMessageLimit();
    if (!limit) return files;
    const kept = [];
    let total = DROP_HEADER;
    for (let i = 0; i < files.length; i++) {
      const f = files[i];
      const need = dropItemOverhead(f) + (f.size || 0);
      if (total + need > limit) {
        dropsRefused++;
        noteSuppressed();
        console.warn('[drop] "' + f.name + '" is ' + f.size +
          ' bytes and the data channel accepts ' + limit + ' per message; not sent');
        continue;
      }
      total += need;
      kept.push(f);
    }
    return kept;
  }

  async function sendDrop(x, y, fileList) {
    const files = screenDrop(Array.from(fileList));
    if (!files.length) return;
    const items = await Promise.all(files.map(async f => ({
      name: enc(f.name),
      mime: enc(f.type || 'application/octet-stream'),
      size: f.size,
      data: new Uint8Array(await fileBytes(f)),
    })));
    let total = 12;
    for (const it of items) total += 2 + it.name.length + 4 + it.mime.length + 8 + 8 + it.data.length;
    const buf = new ArrayBuffer(total);
    const v = new DataView(buf);
    let o = 0;
    v.setUint8(o++, OP.DROP);
    v.setUint8(o++, 4 /* drop */);
    v.setFloat32(o, clamp01(x), true); o += 4;
    v.setFloat32(o, clamp01(y), true); o += 4;
    v.setUint16(o, items.length, true); o += 2;
    for (const it of items) {
      v.setUint16(o, it.name.length, true); o += 2;
      new Uint8Array(buf, o).set(it.name); o += it.name.length;
      v.setUint32(o, it.mime.length, true); o += 4;
      new Uint8Array(buf, o).set(it.mime); o += it.mime.length;
      setU64(v, o, it.size); o += 8;
      setU64(v, o, it.data.length); o += 8;
      new Uint8Array(buf, o).set(it.data); o += it.data.length;
    }
    send(OP.DROP, buf);
  }

  function sendState(op, on) { lifeView.setUint8(0, op); lifeView.setUint8(1, on ? 1 : 0); send(op, lifeBuf); }
  function sendResize() {
    resizeView.setUint8(0, OP.RESIZE);
    resizeView.setUint16(1, Math.min(65535, Math.round(window.innerWidth)), true);
    resizeView.setUint16(3, Math.min(65535, Math.round(window.innerHeight)), true);
    resizeView.setFloat32(5, window.devicePixelRatio || 1, true);
    send(OP.RESIZE, resizeBuf);
  }
  // Client -> host telemetry, v4. Fixed offsets, little-endian. A newer client may append
  // fields and an older host reads the prefix it knows, so the length is a floor, never an
  // equality check. Bytes 0..72 are byte-identical to v1, 0..96 to v2 and 0..104 to v3.
  const STATS_LEN = 120;
  const STATS_VERSION = 4;
  const FLAG = {
    SAB: 1 << 0, ISOLATED: 1 << 1, WAKE_LOCK: 1 << 2, POINTER_LOCK: 1 << 3,
    KEYBOARD_LOCK: 1 << 4, FULLSCREEN: 1 << 5, SET_SINK_ID: 1 << 6,
    DECODER_HW: 1 << 7, CLOCK_VALID: 1 << 8,
  };
  const statsBuf = new ArrayBuffer(STATS_LEN), statsView = new DataView(statsBuf);

  function us(v) {
    if (typeof v !== 'number' || !isFinite(v) || v < 0) return 0;
    const n = Math.round(v * 1000);
    return n > 0xffffffff ? 0xffffffff : n;
  }
  function u32(v) {
    if (typeof v !== 'number' || !isFinite(v) || v < 0) return 0;
    return v > 0xffffffff ? 0xffffffff : (v >>> 0);
  }

  function sendStats() {
    if (!window.SEVideo) return;
    let st;
    try { st = SEVideo.getStats(); } catch (_) { return; }
    if (!st) return;

    let flags = 0;
    if (st.sab) flags |= FLAG.SAB;
    if (st.crossOriginIsolated) flags |= FLAG.ISOLATED;
    if (st.wakeLock) flags |= FLAG.WAKE_LOCK;
    if (st.pointerLock) flags |= FLAG.POINTER_LOCK;
    if (st.keyboardLock) flags |= FLAG.KEYBOARD_LOCK;
    if (st.fullscreen) flags |= FLAG.FULLSCREEN;
    if (st.setSinkId) flags |= FLAG.SET_SINK_ID;
    if (st.decoderHardware) flags |= FLAG.DECODER_HW;
    if (st.clockValid) flags |= FLAG.CLOCK_VALID;

    const a = st.arrivalToPaint || {}, d = st.decode || {}, h = st.hostToPaint || {};
    const v = statsView;
    v.setUint8(0, OP.STATS);
    v.setUint8(1, STATS_VERSION);
    v.setUint8(2, st.videoPath | 0);
    v.setUint8(3, st.audioPath | 0);
    v.setUint32(4, flags >>> 0, true);
    v.setUint32(8, us(a.p50), true);
    v.setUint32(12, us(a.p90), true);
    v.setUint32(16, us(a.p99), true);
    v.setUint32(20, us(d.p50), true);
    v.setUint32(24, us(d.p90), true);
    v.setUint32(28, us(d.p99), true);
    v.setUint32(32, us(h.p50), true);
    v.setUint32(36, us(h.p90), true);
    v.setUint32(40, us(h.p99), true);
    v.setUint32(44, u32(st.framesPainted), true);
    v.setUint32(48, u32(st.framesDropped), true);
    v.setUint32(52, u32(st.decodeErrors), true);
    v.setUint32(56, u32(st.keyRequests), true);
    v.setUint32(60, u32(st.audioUnderruns), true);
    v.setUint32(64, u32(st.audioOverruns), true);
    v.setUint32(68, us(st.rttMs), true);
    // v2 tail: which tier is live on each axis, plus what the page swallowed in a bare catch
    v.setUint8(72, st.transportTier | 0);
    v.setUint8(73, st.renderTier | 0);
    v.setUint8(74, st.keyTier | 0);
    v.setUint8(75, st.sinkTier | 0);
    v.setUint32(76, u32(st.suppressed), true);
    v.setUint32(80, u32(st.degraded) >>> 0, true);
    v.setUint32(84, us(st.audioTargetMs), true);
    v.setUint32(88, u32(st.audioConcealed), true);
    v.setUint32(92, us(st.audioOutputLatencyMs), true);
    // v3 tail: what the host's packet redundancy actually bought. `used` is a hole a copy
    // filled; `discarded` is the copy's cost on a link that did not need it.
    v.setUint32(96, u32(st.audioRedundantUsed), true);
    v.setUint32(100, u32(st.audioRedundantDiscarded), true);
    // v4 tail: a client slower than its stream. Catch-ups (decoder reset to a keyframe because it
    // fell behind), decoded frames not painted because a newer one was ready, the most time a
    // frame lost on this machine over the last second, and where the video path stands now
    // (0 in step, 1 skipping stale paints, 2 behind and painting everything, 3 waiting for a
    // catch-up keyframe). The host can lower the frame rate of a client that keeps reporting 2.
    v.setUint32(104, u32(st.videoCatchUps), true);
    v.setUint32(108, u32(st.videoFramesStale), true);
    v.setUint32(112, us(st.videoLateMs), true);
    v.setUint8(116, st.videoLateState | 0);
    v.setUint8(117, 0);
    v.setUint16(118, 0, true);
    send(OP.STATS, statsBuf);
  }

  function sendPing() {
    pingView.setUint8(0, OP.PING);
    setU64(pingView, 1, Math.round(performance.now() * 1e6));
    send(OP.PING, pingBuf);
  }

  // What the host understands: every PONG carries a u16 bitfield at offset 17, and a frame
  // shorter than 19 bytes is an older host with no bits. Nothing optional may be assumed to
  // work before its bit lands: a request the host drops is not a mechanism.
  const HOST_CAP = {
    KEYFRAME_REQUEST: 1 << 0,
    STATS_V2: 1 << 1,
    UNRELIABLE_MOVE_CHANNEL: 1 << 2,
    INPUT_TIMESTAMP: 1 << 3,
    INPUT_EVENT_FLAGS: 1 << 4,
    INPUT_SEQUENCE: 1 << 5,
  };
  let hostCaps = 0;
  const capSubs = [];
  function setHostCaps(caps) {
    if (caps === hostCaps) return;
    hostCaps = caps;
    for (let i = 0; i < capSubs.length; i++) { try { capSubs[i](caps); } catch (_) {} }
  }
  function noteHostCaps(dv) {
    setHostCaps(dv.byteLength >= 19 ? dv.getUint16(17, true) : 0);
  }

  // session state, in the byte after the capabilities; a host that does not send it reports
  // nothing
  const HOST_STATE = { PICTURE_PENDING: 1 << 0 };
  let hostState = 0;
  // Whether any PONG has been read on this connection: the first one is news even when it says
  // nothing is pending.
  let hostStateSeen = false;
  const stateSubs = [];
  function setHostState(state) {
    if (state === hostState && hostStateSeen) return;
    hostState = state;
    hostStateSeen = true;
    for (let i = 0; i < stateSubs.length; i++) { try { stateSubs[i](state); } catch (_) { noteSuppressed(); } }
  }
  function noteHostState(dv) {
    setHostState(dv.byteLength >= 20 ? dv.getUint8(19) : 0);
  }
  // a new connection starts unknown, quietly: the page clears its own state at teardown
  function resetHostState() {
    hostState = 0;
    hostStateSeen = false;
  }

  // The legacy `createEncodedStreams` transform cannot ask for an IDR, so the page relays the
  // request here, but only to a host that says it parses the opcode. Returning false is what
  // leaves the page's demotion to <video> armed.
  const keyReqBuf = new ArrayBuffer(2), keyReqView = new DataView(keyReqBuf);
  let lastKeyReqAt = 0;
  function requestKeyframe() {
    if (!(hostCaps & HOST_CAP.KEYFRAME_REQUEST)) return false;
    if (!reliable || reliable.readyState !== 'open') return false;
    // start the 200 ms window only when the send will happen: one attempted on a closed
    // channel must not suppress the request that unfreezes the picture
    const now = performance.now();
    if (now - lastKeyReqAt < 200) return false;
    lastKeyReqAt = now;
    keyReqView.setUint8(0, OP.KEYFRAME_REQUEST);
    keyReqView.setUint8(1, 0);
    send(OP.KEYFRAME_REQUEST, keyReqBuf);
    return true;
  }

  // Host clock offset, estimated NTP-style over the once-a-second PING/PONG. The host's clock
  // (`host_now_ns`) is monotonic from its own process start, so a host stamp means nothing here
  // without the distance between the two epochs. `offsetMs` is against absolute time
  // (`performance.timeOrigin + now()`), not `performance.now()`: a dedicated worker's time
  // origin is its own creation, so only the absolute form compares across page and workers.
  const clock = { offsetMs: null, rttMs: null, samples: 0, bestRttMs: Infinity, bestAt: 0 };
  const clockSubs = [];
  // A minimum-RTT sample is the least queued one, so its midpoint estimate is the tightest.
  // Age the best sample out so a route change is not remembered for the whole session.
  const CLOCK_SAMPLE_TTL_MS = 30000;

  function notifyClock() {
    for (let i = 0; i < clockSubs.length; i++) {
      try { clockSubs[i](clock.offsetMs); } catch (_) {}
    }
  }

  function onPong(dv) {
    const echoedNs = getU64(dv, 1);
    const nowMs = performance.now();
    const rttMs = nowMs - echoedNs / 1e6;
    if (dv.byteLength < 17 || !isFinite(rttMs) || rttMs < 0 || rttMs > 5000) return;
    if (!(rttMs <= clock.bestRttMs || nowMs - clock.bestAt > CLOCK_SAMPLE_TTL_MS)) return;
    const hostMs = getU64(dv, 9) / 1e6;
    const midAbsMs = TIME_ORIGIN + (echoedNs / 1e6 + nowMs) / 2;
    const offsetMs = hostMs - midAbsMs;
    if (!isFinite(offsetMs)) { noteSuppressed(); return; }
    clock.bestRttMs = rttMs;
    clock.bestAt = nowMs;
    clock.rttMs = rttMs;
    clock.offsetMs = offsetMs;
    clock.samples++;
    notifyClock();
  }

  function onReliableMessage(ev) {
    try {
      const dv = new DataView(ev.data);
      const op = dv.getUint8(0);
      if (op === OP.PONG && dv.byteLength >= 9) {
        // Read before onPong: the clock estimate keeps only the least-queued sample and
        // returns early on the rest, but the capability bits are the same on every frame.
        noteHostCaps(dv);
        noteHostState(dv);
        onPong(dv);
        if (window.SEHealth) SEHealth.alive();
      } else if (op === OP.BYE) {
        if (window.SEHealth) SEHealth.bye(dv.byteLength >= 2 ? dv.getUint8(1) : 0);
      }
    } catch (_) {}
  }

  const pendingMoves = new Map();
  const pendingStrokes = new Map();
  const rel = { dx: 0, dy: 0, buttons: 0, pending: false };
  let mouseGateOpen = true;
  let absGateOpen = true;
  let flushTimer = 0;

  const flushChannel = new MessageChannel();
  flushChannel.port2.onmessage = () => { if (flushTimer) { flushTimer = 0; flushMoves(); } };

  function armFlush() { if (!flushTimer) { flushTimer = 1; flushChannel.port1.postMessage(null); } }
  function flushStroke(id, st) {
    if (st.samples.length) sendPointerBatch(st.source, id, st.buttons, st.samples);
    st.samples.length = 0;
  }
  function flushMoves() {
    flushTimer = 0;
    if (rel.pending) { sendMouseDelta(rel.dx, rel.dy, rel.buttons); rel.dx = 0; rel.dy = 0; rel.pending = false; }
    if (pendingStrokes.size) { for (const [id, st] of pendingStrokes) flushStroke(id, st); pendingStrokes.clear(); }
    if (pendingMoves.size) { for (const e of pendingMoves.values()) sendPointer(OP.POINTER_MOVE, e); pendingMoves.clear(); }
    mouseGateOpen = true;
    absGateOpen = true;
  }
  // getCoalescedEvents: Chrome 58 / Firefox 59 (always empty on Firefox Android) / Safari 18.2.
  // Without it a high-rate digitiser or 1000 Hz mouse keeps only the last sample per flush.
  const hasCoalesced = window.PointerEvent && ('getCoalescedEvents' in PointerEvent.prototype);
  // the host replays a batch one injection per sample, so cap the burst: eight per flush is
  // about one sample per 2 ms at 60 Hz, finer than any host cursor resolves
  const MAX_COALESCED_PER_FLUSH = 8;
  function coalescedOf(e) {
    if (!hasCoalesced) return [e];
    let c;
    try { c = e.getCoalescedEvents(); } catch (_) { return [e]; }
    if (!c || !c.length) return [e];
    if (c.length <= MAX_COALESCED_PER_FLUSH) return c;
    // keep the newest samples
    return c.slice(c.length - MAX_COALESCED_PER_FLUSH);
  }
  // The full 40-byte pointer frame, coalesced or not: it is the only form that carries contact
  // width, height and per-event buttons. This is the whole of what touch takes.
  function queueSingleMove(e) {
    if (absGateOpen && pendingMoves.size === 0) {
      absGateOpen = false; sendPointer(OP.POINTER_MOVE, e); armFlush(); return;
    }
    pendingMoves.set(e.pointerId, e); armFlush();
  }

  function pushStroke(e, src) {
    let st = pendingStrokes.get(e.pointerId);
    const c = coalescedOf(e);
    if (!st && c.length === 1) {
      // a lone sample goes as the full frame: the 24-byte batch sample has no contact size
      // or per-event buttons
      queueSingleMove(e);
      return;
    }
    if (!st) { st = { source: src, buttons: e.buttons || 0, samples: [] }; pendingStrokes.set(e.pointerId, st); }
    // Any single move already queued for this pointer is older than these samples, and the
    // two queues flush in a fixed order, so it would arrive out of sequence.
    pendingMoves.delete(e.pointerId);
    st.buttons = e.buttons || 0;
    for (let i = 0; i < c.length; i++) st.samples.push(c[i]);
    if (st.samples.length > BATCH_MAX) st.samples.splice(0, st.samples.length - BATCH_MAX);
    if (absGateOpen) { absGateOpen = false; flushStroke(e.pointerId, st); }
    armFlush();
  }

  function onPointerMove(e) {
    const src = sourceByte(e);
    const locked = (document.pointerLockElement === surface || !!document.pointerLockElement);
    if (src === SRC.mouse && locked) {
      // under pointer lock, sum the coalesced samples to recover motion the browser merged away
      let mx = 0, my = 0;
      const c = coalescedOf(e);
      for (let i = 0; i < c.length; i++) { mx += c[i].movementX || 0; my += c[i].movementY || 0; }
      // movementX/Y are not populated on coalesced events in every engine (Firefox leaves them
      // 0, as did Chromium into its 70s): a zero sum means the samples carry no motion, so read
      // the dispatched event instead
      if (mx === 0 && my === 0) { mx = e.movementX || 0; my = e.movementY || 0; }
      const [ax, ay] = applyMouseCurve(mx, my);
      rel.dx += ax; rel.dy += ay; rel.buttons = e.buttons || 0;
      if (mouseGateOpen && !rel.pending) {
        mouseGateOpen = false;
        sendMouseDelta(rel.dx, rel.dy, rel.buttons); rel.dx = 0; rel.dy = 0;
        armFlush();
      } else {
        rel.pending = true; armFlush();
      }
    } else if (src === SRC.touch) {
      updatePinch(e);
      // Touch does not batch: a 24-byte batch sample has no room for the contact width and
      // height the full frame carries at 30 and 34, and the host's batched touch path injects
      // them as zeros, which is wrong for anything reading rcContact (inking, gestures). Pen
      // keeps batching: its samples carry everything the injector uses.
      queueSingleMove(e);
    } else {
      pushStroke(e, src);
    }
  }
  function onPointerDown(e) {
    try { surface.setPointerCapture(e.pointerId); } catch (_) {}
    pendingMoves.delete(e.pointerId);
    pinchStart(e);
    sendPointer(OP.POINTER_DOWN, e, reliable);
  }
  function endPointer(e, op) {
    const st = pendingStrokes.get(e.pointerId);
    if (st) { flushStroke(e.pointerId, st); pendingStrokes.delete(e.pointerId); }
    pendingMoves.delete(e.pointerId);
    pinchEnd(e);
    sendPointer(op, e, reliable);
  }

  const pinchPts = new Map();
  let pinchDist = 0;
  function pinchStart(e) {
    if (sourceByte(e) !== SRC.touch) return;
    pinchPts.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (pinchPts.size === 2) pinchDist = twoFingerDist();
  }
  function updatePinch(e) {
    if (!pinchPts.has(e.pointerId)) return;
    pinchPts.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (pinchPts.size === 2) {
      const d = twoFingerDist();
      if (pinchDist > 0 && d > 0) {
        const r = (d - pinchDist) / pinchDist;
        if (Math.abs(r) > 0.01) { sendZoom(r * 4); pinchDist = d; }
      } else { pinchDist = d; }
    }
  }
  function pinchEnd(e) { pinchPts.delete(e.pointerId); if (pinchPts.size < 2) pinchDist = 0; }
  function twoFingerDist() {
    const it = pinchPts.values(); const a = it.next().value, b = it.next().value;
    return Math.hypot(a.x - b.x, a.y - b.y);
  }
  let gestureScale = 1;

  function isComposingKey(e) { return e.isComposing || e.keyCode === 229; }
  function onKeyDown(e) {
    if (isComposingKey(e)) return;
    sendKey(true, e);
    e.preventDefault();
  }
  function onKeyUp(e) {
    if (isComposingKey(e)) return;
    sendKey(false, e);
    e.preventDefault();
  }

  function clearHeld() {
    pendingMoves.clear();
    pendingStrokes.clear();
    pinchPts.clear(); pinchDist = 0;
    rel.dx = 0; rel.dy = 0; rel.pending = false;
    mouseGateOpen = true; absGateOpen = true;
    if (flushTimer) { flushTimer = 0; }
  }
  // drag and drop normalise against the same letterboxed picture rect as a tap
  function ndx(e) { return normX(e); }
  function ndy(e) { return normY(e); }

  function on(target, type, fn, opts) {
    target.addEventListener(type, (e) => { if (active && !suspended) fn(e); }, opts);
  }

  // While the page has a dialog, a card or the browser's output picker up, pointer and
  // keyboard belong to this page: every listener installed through `on` stands down, and the
  // host is told focus is gone so it lifts any modifier it holds. PING and STATS keep flowing,
  // so a suspended client is still distinguishable from a wedged one.
  function setSuspended(on_) {
    const want = !!on_;
    if (want === suspended) return false;
    suspended = want;
    if (!active) return false;
    if (want) {
      sendState(OP.FOCUS_STATE, false);
      // `pointerlockchange` for the lock the page just dropped is dispatched in a later task,
      // when its listener is already gated off, so tell the host here
      sendState(OP.POINTERLOCK_STATE, false);
      clearHeld();
      return true;
    }
    // sent directly: the gated listener is what is off
    sendState(OP.FOCUS_STATE, true);
    sendState(OP.POINTERLOCK_STATE, !!document.pointerLockElement);
    refreshSize();
    sendResize();
    return true;
  }

  function installListeners() {
    if (installed) return;
    installed = true;
    surface = document.getElementById('stage');
    imeSink = document.getElementById('imeSink');

    if (window.PointerEvent && 'onpointerrawupdate' in window) {
      on(surface, 'pointerrawupdate', onPointerMove);
    } else {
      on(surface, 'pointermove', onPointerMove);
    }
    on(surface, 'pointerdown', onPointerDown);
    on(surface, 'pointerup', e => endPointer(e, OP.POINTER_UP));
    on(surface, 'pointercancel', e => endPointer(e, OP.POINTER_CANCEL));
    on(surface, 'pointerover', e => sendPointer(OP.POINTER_OVER, e));
    on(surface, 'pointerout', e => sendPointer(OP.POINTER_OUT, e));
    on(surface, 'pointerenter', e => sendPointer(OP.POINTER_ENTER, e));
    on(surface, 'pointerleave', e => sendPointer(OP.POINTER_LEAVE, e));

    on(surface, 'wheel', e => { e.preventDefault(); sendWheel(e); }, { passive: false });
    on(surface, 'contextmenu', e => e.preventDefault());

    ['touchstart', 'touchmove', 'touchend', 'touchcancel'].forEach((t) => {
      on(surface, t, e => e.preventDefault(), { passive: false });
    });

    on(surface, 'gesturestart', e => { e.preventDefault(); gestureScale = e.scale || 1; });
    on(surface, 'gesturechange', e => {
      e.preventDefault();
      const s = e.scale || 1;
      if (gestureScale > 0) { sendZoom((s - gestureScale) * 4); }
      gestureScale = s;
    });
    on(surface, 'gestureend', e => e.preventDefault());

    on(window, 'keydown', onKeyDown, true);
    on(window, 'keyup', onKeyUp, true);

    if (imeSink) {
      on(imeSink, 'compositionupdate', e => sendText(OP.COMPOSITION_UPDATE, e.data || ''));
      on(imeSink, 'compositionend', e => { sendText(OP.TEXT_INPUT, e.data || ''); imeSink.value = ''; });
      on(imeSink, 'input', e => {
        if (e.isComposing) return;
        if (e.inputType && (e.inputType.startsWith('insertComposition') || e.inputType === 'insertFromPaste')) {
          sendText(OP.TEXT_INPUT, e.data || '');
        }
        imeSink.value = '';
      });
    }

    on(window, 'copy', e => forwardClipboard(0, e));
    on(window, 'cut', e => forwardClipboard(1, e));
    on(window, 'paste', e => forwardClipboard(2, e));

    on(surface, 'dragenter', e => { e.preventDefault(); sendDrag(1, ndx(e), ndy(e)); });
    on(surface, 'dragover', e => { e.preventDefault(); sendDrag(2, ndx(e), ndy(e)); });
    on(surface, 'dragleave', e => { e.preventDefault(); sendDrag(3, ndx(e), ndy(e)); });
    on(surface, 'drop', e => {
      e.preventDefault();
      if (e.dataTransfer && e.dataTransfer.files && e.dataTransfer.files.length) sendDrop(ndx(e), ndy(e), e.dataTransfer.files).catch(noteSuppressed);
      else sendDrag(4, ndx(e), ndy(e));
    });

    on(window, 'blur', () => { sendState(OP.FOCUS_STATE, false); clearHeld(); });
    on(window, 'focus', () => sendState(OP.FOCUS_STATE, true));
    on(document, 'visibilitychange', () => {
      const visible = document.visibilityState === 'visible';
      sendState(OP.VISIBILITY, visible);
      if (!visible) clearHeld();
    });
    on(document, 'pointerlockchange', () => {
      const locked = document.pointerLockElement === surface || !!document.pointerLockElement;
      // losing the lock withdraws the unadjustedMovement claim, so a re-entry by another route
      // cannot tell the host its OS-curved deltas are raw
      if (!locked) lockIsUnadjusted = false;
      sendState(OP.POINTERLOCK_STATE, locked);
    });

    on(window, 'resize', refreshSize, { passive: true });
    on(window, 'orientationchange', refreshSize, { passive: true });
  }

  // Focusing `#imeSink` (a real off-screen text input) raises the software keyboard on a touch
  // device, so only do it where there is a hardware one. `maxTouchPoints === 0` excludes
  // touchscreen laptops; `(any-pointer: fine)` alone matches a pen (an iPad with a Pencil, a
  // Surface stylus). Hover separates them: a mouse or trackpad reports it, a digitiser does not.
  function hasFinePointer() {
    try {
      if (window.matchMedia) {
        return window.matchMedia('(any-pointer: fine)').matches &&
          window.matchMedia('(any-hover: hover)').matches;
      }
    } catch (_) { noteSuppressed(); }
    return navigator.maxTouchPoints === 0;
  }

  function forwardClipboard(opByte, e) {
    const cd = e.clipboardData;
    if (!cd) return;
    const text = cd.getData('text/plain');
    if (text) sendClipboard(opByte, 'text/plain', enc(text));
  }

  function setup(pc) {
    if (!pc || typeof pc.createDataChannel !== 'function') return;
    dropPc = pc;
    try {
      fast = pc.createDataChannel('fast', { ordered: false, maxRetransmits: 0, priority: 'high' });
      reliable = pc.createDataChannel('reliable', { ordered: true, priority: 'high' });
      bulk = pc.createDataChannel('bulk', { ordered: true });
    } catch (_) { return; }
    fast.binaryType = 'arraybuffer';
    reliable.binaryType = 'arraybuffer';
    bulk.binaryType = 'arraybuffer';
    reliable.onmessage = onReliableMessage;
    reliable.onopen = () => {
      refreshSize();
      sendResize();
      // only where there is a hardware keyboard (see hasFinePointer)
      if (imeSink && hasFinePointer()) {
        try { imeSink.focus({ preventScroll: true }); } catch (_) { noteSuppressed(); }
      }
      if (!pingTimer) {
        // ping at once, not a second in: its PONG's state byte holds the page's no-picture
        // timers, which start at connect
        try { sendPing(); } catch (_) { noteSuppressed(); }
        // separate guards: a throw in sendStats() must not cost the clock sample, or the reverse
        pingTimer = setInterval(function () {
          try { sendPing(); } catch (_) { noteSuppressed(); }
          try { sendStats(); } catch (_) { noteSuppressed(); }
        }, 1000);
      }
    };
    reliable.onclose = () => { if (pingTimer) { clearInterval(pingTimer); pingTimer = 0; } clearHeld(); };
    installListeners();
    active = true;
  }

  function teardown() {
    active = false;
    suspended = false;
    if (pingTimer) { clearInterval(pingTimer); pingTimer = 0; }
    clearHeld();
    [fast, reliable, bulk].forEach((ch) => { try { if (ch) ch.close(); } catch (_) {} });
    fast = reliable = bulk = null;
    dropPc = null;
    // the next connection's host answers with its own PONG (a reconfigure can land on a
    // different adapter), so the tiers gated on these bits are withdrawn until it does
    setHostCaps(0);
    resetHostState();
    lockIsUnadjusted = false;
    if (imeSink) {
      imeSink.value = '';
      try { imeSink.blur(); } catch (_) {}
    }
  }

  function noteSuppressed() {
    if (window.SEVideo && SEVideo.noteSuppressed) SEVideo.noteSuppressed();
  }

  window.RemoteInput = {
    setup,
    teardown,
    setSuspended,
    isSuspended() { return suspended; },
    requestKeyframe,
    setPictureSize,
    hostCapabilities() { return hostCaps; },
    hostCapability(name) { return !!(hostCaps & (HOST_CAP[name] || 0)); },
    picturePending() { return !!(hostState & HOST_STATE.PICTURE_PENDING); },
    onHostState(fn) {
      if (typeof fn !== 'function') return;
      stateSubs.push(fn);
      if (hostStateSeen) { try { fn(hostState); } catch (_) { noteSuppressed(); } }
    },
    onHostCapabilities(fn) {
      if (typeof fn !== 'function') return;
      capSubs.push(fn);
      if (hostCaps) { try { fn(hostCaps); } catch (_) {} }
    },
    hasCoalescedEvents() { return !!hasCoalesced; },
    // the page owns the pointer-lock request, so only it knows whether `unadjustedMovement`
    // was granted
    setPointerLockUnadjusted(on) { lockIsUnadjusted = !!on; },
    get fastDropped() { return fastDropped; },
    onViewportSettled() { refreshSize(); sendResize(); },
  };

  // `offsetMs` converts between the host clock and absolute local time:
  //   hostMs = absMs + offsetMs        absMs = hostMs - offsetMs
  // Null until the first PONG carrying a host stamp lands (about a second into a session,
  // and never at all against a host too old to send one).
  window.SEClock = {
    get offsetMs() { return clock.offsetMs; },
    get rttMs() { return clock.rttMs; },
    get samples() { return clock.samples; },
    hostNsToAbsMs(hostNs) {
      return clock.offsetMs === null ? null : hostNs / 1e6 - clock.offsetMs;
    },
    subscribe(fn) {
      if (typeof fn !== 'function') return;
      clockSubs.push(fn);
      if (clock.offsetMs !== null) { try { fn(clock.offsetMs); } catch (_) {} }
    },
  };
})();

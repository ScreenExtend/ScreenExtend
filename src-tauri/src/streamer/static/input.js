'use strict';
(function () {
  const OP = {
    POINTER_DOWN: 0x01, POINTER_UP: 0x02, POINTER_MOVE: 0x03, POINTER_CANCEL: 0x04,
    POINTER_ENTER: 0x05, POINTER_LEAVE: 0x06, POINTER_OVER: 0x07, POINTER_OUT: 0x08,
    POINTER_MOVE_BATCH: 0x09,
    WHEEL: 0x10, ZOOM: 0x11, KEY: 0x20, TEXT_INPUT: 0x21, COMPOSITION_UPDATE: 0x22,
    CLIPBOARD: 0x30, DRAG: 0x40, DROP: 0x41,
    FOCUS_STATE: 0x50, VISIBILITY: 0x51, RESIZE: 0x52, POINTERLOCK_STATE: 0x53,
    MOUSE_DELTA: 0x54, PING: 0x60, PONG: 0x61, STATS: 0x62,
  };
  const SRC = { mouse: 0x00, touch: 0x01, pen: 0x02 };
  const te = new TextEncoder();
  const mouseRelative = true;
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
  let surface = null;
  let imeSink = null;

  let cssW = window.innerWidth, cssH = window.innerHeight;
  function refreshSize() {
    cssW = (surface && surface.clientWidth) || window.innerWidth;
    cssH = (surface && surface.clientHeight) || window.innerHeight;
  }

  const moveBuf = new ArrayBuffer(40), moveView = new DataView(moveBuf);
  const wheelBuf = new ArrayBuffer(15), wheelView = new DataView(wheelBuf);
  const deltaBuf = new ArrayBuffer(9), deltaView = new DataView(deltaBuf);
  const resizeBuf = new ArrayBuffer(9), resizeView = new DataView(resizeBuf);
  const lifeBuf = new ArrayBuffer(2), lifeView = new DataView(lifeBuf);
  const pingBuf = new ArrayBuffer(9), pingView = new DataView(pingBuf);
  const zoomBuf = new ArrayBuffer(5), zoomView = new DataView(zoomBuf);
  const keyScratch = new Uint8Array(1024), keyView = new DataView(keyScratch.buffer);
  const BATCH_MAX = 40;
  const batchBuf = new ArrayBuffer(10 + BATCH_MAX * 24), batchView = new DataView(batchBuf);

  function clamp01(v) { return v < 0 ? 0 : v > 1 ? 1 : v; }
  function clampI16(v) { return v < -32768 ? -32768 : v > 32767 ? 32767 : (v | 0); }

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
  function rawSend(ch, buf, len) {
    if (!ch || ch.readyState !== 'open') return;
    try { ch.send(len === undefined ? buf : new Uint8Array(buf, 0, len)); } catch (_) {}
  }
  function send(op, buf, len) { rawSend(channelFor(op), buf, len); }

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
  function normX(e) { return clamp01(e.clientX / cssW); }
  function normY(e) { return clamp01(e.clientY / cssH); }

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
    moveView.setFloat32(30, (e.width || 0) / cssW, true);
    moveView.setFloat32(34, (e.height || 0) / cssH, true);
    moveView.setUint16(38, e.buttons || 0, true);
    if (ch) rawSend(ch, moveBuf); else send(op, moveBuf);
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
        batchView.setFloat32(o, clamp01(s.clientX / cssW), true);
        batchView.setFloat32(o + 4, clamp01(s.clientY / cssH), true);
        batchView.setFloat32(o + 8, s.pressure != null ? s.pressure : 0, true);
        batchView.setFloat32(o + 12, s.tiltX || 0, true);
        batchView.setFloat32(o + 16, s.tiltY || 0, true);
        batchView.setFloat32(o + 20, s.twist || 0, true);
        o += 24;
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
    send(OP.WHEEL, wheelBuf);
  }
  function sendMouseDelta(dx, dy, buttons) {
    deltaView.setUint8(0, OP.MOUSE_DELTA);
    deltaView.setUint8(1, 0);
    deltaView.setInt16(2, clampI16(dx), true);
    deltaView.setInt16(4, clampI16(dy), true);
    deltaView.setUint16(6, buttons || 0, true);
    deltaView.setUint8(8, 0);
    send(OP.MOUSE_DELTA, deltaBuf);
  }

  function sendKey(down, e) {
    const code = te.encode(e.code || '');
    const key = te.encode(e.key || '');
    if (code.length > 255 || key.length > 255) return;
    let o = 0;
    keyView.setUint8(o++, OP.KEY);
    keyView.setUint8(o++, (down ? 1 : 0) | (e.repeat ? 2 : 0));
    keyView.setUint16(o, modMask(e), true); o += 2;
    keyView.setUint8(o++, code.length);
    keyScratch.set(code, o); o += code.length;
    keyView.setUint8(o++, key.length);
    keyScratch.set(key, o); o += key.length;
    send(OP.KEY, keyScratch.buffer, o);
  }

  function sendText(op, str) {
    if (!str) return;
    const data = te.encode(str);
    const buf = new ArrayBuffer(5 + data.length);
    const v = new DataView(buf);
    v.setUint8(0, op);
    v.setUint32(1, data.length, true);
    new Uint8Array(buf, 5).set(data);
    send(op, buf);
  }

  function sendClipboard(opByte, mime, dataBytes) {
    const m = te.encode(mime);
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
  async function sendDrop(x, y, fileList) {
    const files = Array.from(fileList);
    const items = await Promise.all(files.map(async f => ({
      name: te.encode(f.name),
      mime: te.encode(f.type || 'application/octet-stream'),
      size: BigInt(f.size),
      data: new Uint8Array(await f.arrayBuffer()),
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
      v.setBigUint64(o, it.size, true); o += 8;
      v.setBigUint64(o, BigInt(it.data.length), true); o += 8;
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
  function sendPing() {
    const t = BigInt(Math.round(performance.now() * 1e6));
    pingView.setUint8(0, OP.PING);
    pingView.setBigUint64(1, t, true);
    send(OP.PING, pingBuf);
  }
  let lastRttMs = 0;
  function onReliableMessage(ev) {
    try {
      const dv = new DataView(ev.data);
      if (dv.getUint8(0) === OP.PONG && dv.byteLength >= 9) {
        const echoed = dv.getBigUint64(1, true);
        const now = BigInt(Math.round(performance.now() * 1e6));
        lastRttMs = Number(now - echoed) / 1e6;
      }
    } catch (_) {}
  }

  const TICK_MS = 8;
  const pendingMoves = new Map();
  const pendingStrokes = new Map();
  const rel = { dx: 0, dy: 0, buttons: 0, pending: false };
  let mouseGateOpen = true;
  let absGateOpen = true;
  let flushTimer = 0;

  function armFlush() { if (!flushTimer) flushTimer = setTimeout(flushMoves, TICK_MS); }
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
  function coalescedOf(e) {
    return (typeof e.getCoalescedEvents === 'function') ? (e.getCoalescedEvents() || [e]) : [e];
  }

  function onPointerMove(e) {
    const src = sourceByte(e);
    if (src === SRC.mouse && mouseRelative) {
      const [ax, ay] = applyMouseCurve(e.movementX || 0, e.movementY || 0);
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
      if (absGateOpen && pendingMoves.size === 0) {
        absGateOpen = false; sendPointer(OP.POINTER_MOVE, e); armFlush();
      } else {
        pendingMoves.set(e.pointerId, e); armFlush();
      }
    } else {
      let st = pendingStrokes.get(e.pointerId);
      if (!st) { st = { source: src, buttons: e.buttons || 0, samples: [] }; pendingStrokes.set(e.pointerId, st); }
      st.buttons = e.buttons || 0;
      const c = coalescedOf(e);
      for (let i = 0; i < c.length; i++) st.samples.push(c[i]);
      if (absGateOpen) { absGateOpen = false; flushStroke(e.pointerId, st); armFlush(); }
      else { armFlush(); }
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
    if (flushTimer) { clearTimeout(flushTimer); flushTimer = 0; }
  }
  function ndx(e) { return clamp01(e.clientX / (cssW || window.innerWidth)); }
  function ndy(e) { return clamp01(e.clientY / (cssH || window.innerHeight)); }

  function installListeners() {
    if (installed) return;
    installed = true;
    surface = document.getElementById('stage');
    imeSink = document.getElementById('imeSink');
    refreshSize();

    surface.addEventListener('pointermove', onPointerMove);
    surface.addEventListener('pointerdown', onPointerDown);
    surface.addEventListener('pointerup', e => endPointer(e, OP.POINTER_UP));
    surface.addEventListener('pointercancel', e => endPointer(e, OP.POINTER_CANCEL));
    surface.addEventListener('pointerover', e => sendPointer(OP.POINTER_OVER, e));
    surface.addEventListener('pointerout', e => sendPointer(OP.POINTER_OUT, e));
    surface.addEventListener('pointerenter', e => sendPointer(OP.POINTER_ENTER, e));
    surface.addEventListener('pointerleave', e => sendPointer(OP.POINTER_LEAVE, e));

    surface.addEventListener('wheel', e => { e.preventDefault(); sendWheel(e); }, { passive: false });
    surface.addEventListener('contextmenu', e => e.preventDefault());

    for (const t of ['touchstart', 'touchmove', 'touchend', 'touchcancel']) {
      surface.addEventListener(t, e => e.preventDefault(), { passive: false });
    }

    surface.addEventListener('gesturestart', e => { e.preventDefault(); gestureScale = e.scale || 1; });
    surface.addEventListener('gesturechange', e => {
      e.preventDefault();
      const r = ((e.scale || 1) - gestureScale) / (gestureScale || 1);
      if (Math.abs(r) > 0.01) { sendZoom(r * 4); gestureScale = e.scale || 1; }
    });
    surface.addEventListener('gestureend', e => e.preventDefault());

    window.addEventListener('keydown', onKeyDown, true);
    window.addEventListener('keyup', onKeyUp, true);

    if (imeSink) {
      imeSink.addEventListener('compositionupdate', e => sendText(OP.COMPOSITION_UPDATE, e.data || ''));
      imeSink.addEventListener('compositionend', e => { sendText(OP.TEXT_INPUT, e.data || ''); imeSink.value = ''; });
      imeSink.addEventListener('input', e => {
        if (e.isComposing) return;
        if (e.inputType && (e.inputType.startsWith('insertComposition') || e.inputType === 'insertFromPaste')) {
          sendText(OP.TEXT_INPUT, e.data || '');
        }
        imeSink.value = '';
      });
    }

    window.addEventListener('copy', e => forwardClipboard(0, e));
    window.addEventListener('cut', e => forwardClipboard(1, e));
    window.addEventListener('paste', e => forwardClipboard(2, e));

    surface.addEventListener('dragenter', e => { e.preventDefault(); sendDrag(1, ndx(e), ndy(e)); });
    surface.addEventListener('dragover', e => { e.preventDefault(); sendDrag(2, ndx(e), ndy(e)); });
    surface.addEventListener('dragleave', e => { e.preventDefault(); sendDrag(3, ndx(e), ndy(e)); });
    surface.addEventListener('drop', e => {
      e.preventDefault();
      if (e.dataTransfer && e.dataTransfer.files && e.dataTransfer.files.length) sendDrop(ndx(e), ndy(e), e.dataTransfer.files);
      else sendDrag(4, ndx(e), ndy(e));
    });

    window.addEventListener('blur', () => { sendState(OP.FOCUS_STATE, false); clearHeld(); });
    window.addEventListener('focus', () => sendState(OP.FOCUS_STATE, true));
    document.addEventListener('visibilitychange', () => {
      const visible = document.visibilityState === 'visible';
      sendState(OP.VISIBILITY, visible);
      if (!visible) clearHeld();
    });
    document.addEventListener('pointerlockchange', () => {
      sendState(OP.POINTERLOCK_STATE, document.pointerLockElement === surface || !!document.pointerLockElement);
    });

    window.addEventListener('resize', () => { refreshSize(); sendResize(); }, { passive: true });
    window.addEventListener('orientationchange', () => { refreshSize(); sendResize(); }, { passive: true });
  }

  function forwardClipboard(opByte, e) {
    const cd = e.clipboardData;
    if (!cd) return;
    const text = cd.getData('text/plain');
    if (text) sendClipboard(opByte, 'text/plain', te.encode(text));
  }

  function setup(pc) {
    if (!pc || typeof pc.createDataChannel !== 'function') return;
    try {
      fast = pc.createDataChannel('fast', { ordered: false, maxRetransmits: 0 });
      reliable = pc.createDataChannel('reliable', { ordered: true });
      bulk = pc.createDataChannel('bulk', { ordered: true });
    } catch (_) { return; }
    fast.binaryType = 'arraybuffer';
    reliable.binaryType = 'arraybuffer';
    bulk.binaryType = 'arraybuffer';
    reliable.onmessage = onReliableMessage;
    reliable.onopen = () => {
      refreshSize();
      sendResize();
      if (imeSink && navigator.maxTouchPoints === 0) {
        try { imeSink.focus({ preventScroll: true }); } catch (_) {}
      }
      if (!pingTimer) pingTimer = setInterval(sendPing, 1000);
    };
    reliable.onclose = () => { if (pingTimer) { clearInterval(pingTimer); pingTimer = 0; } clearHeld(); };
    installListeners();
  }

  window.RemoteInput = {
    setup,
    get rtt() { return lastRttMs; },
  };
})();

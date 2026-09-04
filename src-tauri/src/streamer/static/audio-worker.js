'use strict';

const SR = 48000;
const HDR_BYTES = 13;
const SYNC_POST_INTERVAL_MS = 250;

const CTRL_INTS = 16;
const CTRL_BYTES = CTRL_INTS * 4;
const W = 0;
const R = 1;
const OVERRUNS = 2;

let ctrl = null;
let ringL = null;
let ringR = null;
let capFrames = 0;
let mask = 0;
let shared = false;

let decoder = null;
let lastSeq = null;
let preEnqueueEmaMs = null;
let lastSyncPostAt = 0;
let scratchL = null;
let scratchR = null;

function setupShared(sab, frames) {
    ctrl = new Int32Array(sab, 0, CTRL_INTS);
    ringL = new Float32Array(sab, CTRL_BYTES, frames);
    ringR = new Float32Array(sab, CTRL_BYTES + frames * 4, frames);
    capFrames = frames;
    mask = frames - 1;
    shared = true;
}

function writeShared(l, r, frames) {
    const w = Atomics.load(ctrl, W) >>> 0;
    const rd = Atomics.load(ctrl, R) >>> 0;
    const free = capFrames - ((w - rd) >>> 0);
    let n = frames;
    if (n > free) {
        Atomics.add(ctrl, OVERRUNS, 1);
        n = free;
        if (n <= 0) return;
    }
    const start = w & mask;
    const first = Math.min(n, capFrames - start);
    ringL.set(l.subarray(0, first), start);
    ringR.set(r.subarray(0, first), start);
    if (first < n) {
        ringL.set(l.subarray(first, n), 0);
        ringR.set(r.subarray(first, n), 0);
    }
    Atomics.store(ctrl, W, (w + n) >>> 0);
}

function postSamples(l, r, frames) {
    const cl = new Float32Array(frames);
    const cr = new Float32Array(frames);
    cl.set(l);
    cr.set(r);
    self.postMessage({ type: 'samples', l: cl, r: cr }, [cl.buffer, cr.buffer]);
}

function onDecoded(ad) {
    const frames = ad.numberOfFrames;
    if (!frames) {
        ad.close();
        return;
    }
    const chs = ad.numberOfChannels;
    const captureHostMs = ad.timestamp / 1000;

    if (!scratchL || scratchL.length < frames) {
        scratchL = new Float32Array(frames);
        scratchR = new Float32Array(frames);
    }
    const l = scratchL.subarray(0, frames);
    const r = scratchR.subarray(0, frames);
    try {
        ad.copyTo(l, { planeIndex: 0, format: 'f32-planar' });
        if (chs >= 2) ad.copyTo(r, { planeIndex: 1, format: 'f32-planar' });
        else r.set(l);
    } catch (e) {
        ad.close();
        return;
    }
    ad.close();

    const enqueueAbsMs = performance.timeOrigin + performance.now();
    const pre = enqueueAbsMs - captureHostMs;
    preEnqueueEmaMs = (preEnqueueEmaMs === null) ? pre : preEnqueueEmaMs * 0.9 + pre * 0.1;
    if (enqueueAbsMs - lastSyncPostAt > SYNC_POST_INTERVAL_MS) {
        lastSyncPostAt = enqueueAbsMs;
        self.postMessage({ type: 'sync', preEnqueueMs: preEnqueueEmaMs });
    }

    if (shared) writeShared(l, r, frames);
    else postSamples(l, r, frames);
}

function makeDecoder() {
    try {
        decoder = new AudioDecoder({
            output: onDecoded,
            error: (e) => {
                console.warn('[audio-worker] decoder error:', e);
                try { if (decoder && decoder.state !== 'closed') decoder.close(); } catch (_) {}
                decoder = null;
                lastSeq = null;
                makeDecoder();
            },
        });
        decoder.configure({ codec: 'opus', sampleRate: SR, numberOfChannels: 2 });
    } catch (e) {
        console.warn('[audio-worker] decoder unavailable:', e);
        decoder = null;
    }
}

function handlePacket(buf) {
    if (buf.byteLength < HDR_BYTES) return;
    const dv = new DataView(buf);
    const seq = dv.getUint32(0, true);
    const captureNs = dv.getBigUint64(4, true);
    if (lastSeq !== null) {
        if (seq === lastSeq) return; // duplicate
        if (((seq - lastSeq) >>> 0) >= 0x80000000) return; // older --> late
    }
    lastSeq = seq;
    const opus = new Uint8Array(buf, HDR_BYTES);
    if (opus.byteLength === 0) return;
    if (!decoder || decoder.state !== 'configured') return;
    try {
        decoder.decode(new EncodedAudioChunk({
            type: 'key',
            timestamp: Number(captureNs / 1000n), // micro-s
            data: opus,
        }));
    } catch (e) {
        console.warn('[audio-worker] decode failed:', e);
    }
}

self.onmessage = (ev) => {
    const d = ev.data;
    if (d instanceof ArrayBuffer) {
        handlePacket(d);
        return;
    }
    if (!d) return;
    if (d.type === 'init') {
        lastSeq = null;
        preEnqueueEmaMs = null;
        if (d.sab) setupShared(d.sab, d.capFrames);
        makeDecoder();
    }
};

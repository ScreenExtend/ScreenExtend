'use strict';

const SR = 48000;
// seq u32 LE, capture_ns u64 LE, flags u8 — see streamer/audio/protocol.rs, which is the
// authority on this layout.
const HDR_BYTES = 13;
const FLAG_SILENT = 1 << 0;
const FLAG_DISCONTINUITY = 1 << 1;
const SYNC_POST_INTERVAL_MS = 250;
// The host encodes 5 ms frames; a seq gap is that many frames of missing audio.
const FRAME_MS = 5;
const FRAMES_PER_PACKET = SR * FRAME_MS / 1000;
// Beyond this a gap is a stall, not a loss: concealing it would just be noise.
const MAX_CONCEAL_PACKETS = 6;

// This file is only ever loaded once AudioDecoder has been probed and answered yes, which is
// why it may use syntax and APIs the rest of the client page may not. It still avoids BigInt:
// the u64 capture stamp is read as two u32 halves, exactly the bytes getBigUint64 would see.
const U32 = 4294967296;
function readU64(dv, off) {
    return dv.getUint32(off, true) + dv.getUint32(off + 4, true) * U32;
}

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
// Host-to-local clock offset from the page (see input.js). 0 until the first PONG carrying a
// host stamp lands, which reproduces the old epoch-biased number; the video worker carries
// the identical bias, so the A/V sync difference is unaffected either way.
let hostOffsetMs = 0;
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
    const pre = enqueueAbsMs - (captureHostMs - hostOffsetMs);
    // Same asymmetric min-tracking estimator as transform-worker.js: the A/V target is the
    // difference of these two numbers, so a mismatched filter would bias it for no reason.
    if (preEnqueueEmaMs === null) {
        preEnqueueEmaMs = pre;
    } else {
        const a = pre < preEnqueueEmaMs ? 0.30 : 0.05;
        preEnqueueEmaMs = preEnqueueEmaMs * (1 - a) + pre * a;
    }
    if (enqueueAbsMs - lastSyncPostAt > SYNC_POST_INTERVAL_MS) {
        lastSyncPostAt = enqueueAbsMs;
        self.postMessage({ type: 'sync', preEnqueueMs: preEnqueueEmaMs, concealed: concealed });
    }

    rememberDecoded(l, r, frames);
    if (shared) writeShared(l, r, frames);
    else postSamples(l, r, frames);
}

// A decoder that errors on every packet must not be rebuilt forever: past this the page is
// told, so it can drop to the browser's own Opus track instead of playing silence.
const MAX_DECODER_RESETS = 3;
let decoderResets = 0;

function makeDecoder() {
    try {
        decoder = new AudioDecoder({
            output: onDecoded,
            error: (e) => {
                console.warn('[audio-worker] decoder error:', e);
                try { if (decoder && decoder.state !== 'closed') decoder.close(); } catch (_) {}
                decoder = null;
                lastSeq = null;
                decoderResets++;
                if (decoderResets > MAX_DECODER_RESETS) {
                    self.postMessage({ type: 'decoder-failed', message: String(e) });
                    return;
                }
                makeDecoder();
            },
        });
        decoder.configure({ codec: 'opus', sampleRate: SR, numberOfChannels: 2 });
    } catch (e) {
        console.warn('[audio-worker] decoder unavailable:', e);
        decoder = null;
        self.postMessage({ type: 'decoder-failed', message: String(e) });
    }
}

// Conceal a loss rather than let the ring starve. The worklet's own concealment only sees an
// empty ring — by then the depth has already collapsed and it re-primes, which costs a whole
// buffer of silence. Filling the hole here keeps the ring at depth and the loss inaudible.
let concealed = 0;
let lastDecodedL = null;
let lastDecodedR = null;
let lastDecodedFrames = 0;

function concealPackets(n) {
    if (!lastDecodedFrames || n <= 0) return;
    const count = Math.min(n, MAX_CONCEAL_PACKETS);
    for (let p = 0; p < count; p++) {
        const frames = lastDecodedFrames;
        if (!scratchL || scratchL.length < frames) {
            scratchL = new Float32Array(frames);
            scratchR = new Float32Array(frames);
        }
        const l = scratchL.subarray(0, frames);
        const r = scratchR.subarray(0, frames);
        // Attenuate as the gap widens: a repeated waveform is convincing for a few
        // milliseconds and buzzy after that.
        const g = Math.pow(0.55, p + 1);
        for (let i = 0; i < frames; i++) {
            l[i] = lastDecodedL[i] * g;
            r[i] = lastDecodedR[i] * g;
        }
        concealed++;
        if (shared) writeShared(l, r, frames);
        else postSamples(l, r, frames);
    }
}

function rememberDecoded(l, r, frames) {
    if (!lastDecodedL || lastDecodedL.length < frames) {
        lastDecodedL = new Float32Array(frames);
        lastDecodedR = new Float32Array(frames);
    }
    lastDecodedL.set(l.subarray(0, frames));
    lastDecodedR.set(r.subarray(0, frames));
    lastDecodedFrames = frames;
}

function handlePacket(buf) {
    if (buf.byteLength < HDR_BYTES) return;
    const dv = new DataView(buf);
    const seq = dv.getUint32(0, true);
    const captureNs = readU64(dv, 4);
    const flags = dv.getUint8(12);
    if (lastSeq !== null) {
        if (seq === lastSeq) return; // duplicate
        if (((seq - lastSeq) >>> 0) >= 0x80000000) return; // older --> late
        const gap = ((seq - lastSeq) >>> 0) - 1;
        if (gap > 0 && !(flags & FLAG_DISCONTINUITY)) concealPackets(gap);
    }
    lastSeq = seq;
    // The host is telling us the capture itself broke, so there is nothing to bridge: let
    // the worklet resynchronise instead of pretending the missing audio existed.
    if (flags & FLAG_DISCONTINUITY) {
        lastDecodedFrames = 0;
        self.postMessage({ type: 'discontinuity' });
    }
    const opus = new Uint8Array(buf, HDR_BYTES);
    if (opus.byteLength === 0) return;
    if (!decoder || decoder.state !== 'configured') return;
    try {
        decoder.decode(new EncodedAudioChunk({
            type: 'key',
            timestamp: Math.round(captureNs / 1000), // micro-s
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
        concealed = 0;
        decoderResets = 0;
        lastDecodedFrames = 0;
        if (d.sab) setupShared(d.sab, d.capFrames);
        makeDecoder();
    } else if (d.type === 'clockoffset') {
        if (typeof d.offsetMs === 'number' && isFinite(d.offsetMs)) {
            if (hostOffsetMs === 0) preEnqueueEmaMs = null;
            hostOffsetMs = d.offsetMs;
        }
    }
};

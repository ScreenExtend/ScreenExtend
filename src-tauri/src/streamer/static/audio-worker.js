'use strict';

const SR = 48000;
// seq u32 LE, capture_ns u64 LE, flags u8 — see streamer/audio/protocol.rs, which is the
// authority on this layout.
const HDR_BYTES = 13;
const FLAG_SILENT = 1 << 0;
const FLAG_DISCONTINUITY = 1 << 1;
// The body carries two payloads: a u16 LE length, this frame's Opus packet, then the previous
// frame's. RFC 2198-style redundancy without the RTP framing, and the only loss-robustness
// tool this path has — the host's encoder is CELT-only at 5 ms, where libopus's in-band FEC is
// structurally inert. The host sets it only when the join advertised `redundancy`, so an older
// client is never handed a body it cannot split.
const FLAG_REDUNDANT = 1 << 2;
const REDUNDANCY_PREFIX_LEN = 2;
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

// Planar audio ring ABI, writer side. Hand-duplicated in audio.js (which owns the allocation
// size) and audio-worklet.js (the reader); change all three together.
//
//   [0 .. 64)                        control block, 16 Int32 (W, R, OVERRUNS)
//   [64 .. 64 + cap*4)               left channel,  Float32
//   [64 + cap*4 .. 64 + cap*8)       right channel, Float32
//
// Positions are absolute uint32 frame counters indexed with `pos & (cap-1)`, so the capacity
// must be a power of two.
function clearShared() {
    ctrl = null;
    ringL = null;
    ringR = null;
    capFrames = 0;
    mask = 0;
    shared = false;
}

function setupShared(sab, frames) {
    if (!frames || (frames & (frames - 1)) !== 0) {
        console.error('[audio-worker] ring capacity is not a power of two:', frames);
        return;
    }
    if (sab.byteLength < CTRL_BYTES + frames * 8) {
        console.error('[audio-worker] ring is too small for', frames, 'frames:', sab.byteLength);
        return;
    }
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

// Set once the page has handed over one end of a MessageChannel whose other end the worklet
// took. Until then — and on any engine that will not transfer a port into an
// AudioWorkletGlobalScope — samples go to the page, which re-posts them to the worklet.
let samplePort = null;

function postSamples(l, r, frames) {
    const cl = new Float32Array(frames);
    const cr = new Float32Array(frames);
    cl.set(l);
    cr.set(r);
    const msg = { type: 'samples', l: cl, r: cr };
    const transfer = [cl.buffer, cr.buffer];
    if (samplePort) samplePort.postMessage(msg, transfer);
    else self.postMessage(msg, transfer);
}
// The worklet reads these on the same port, so they must not overtake or fall behind the
// samples they describe.
function postToWorklet(msg) {
    if (samplePort) samplePort.postMessage(msg);
    else self.postMessage(msg);
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
        // A copyTo that refuses the format conversion drops every packet the decoder
        // produces. Swallowed frame by frame it was permanent silence with the client still
        // reporting the fast audio path and audioUnderruns climbing for no visible reason.
        copyFailures++;
        if (copyFailures === MAX_COPY_FAILURES) {
            self.postMessage({
                type: 'decoder-failed',
                reason: 'copyTo',
                message: 'AudioData.copyTo refused f32-planar: ' + e,
            });
        }
        return;
    }
    ad.close();
    copyFailures = 0;
    // "Three failures in a row", the way the video worker counts them — not three in the
    // lifetime of a four-hour session with one transient decode error an hour.
    decoderResets = 0;

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
        self.postMessage({
            type: 'sync',
            preEnqueueMs: preEnqueueEmaMs,
            concealed: concealed,
            redundantUsed: redundantUsed,
            redundantDiscarded: redundantDiscarded,
        });
    }

    // Anything owed in front of this packet goes in first, from this same task, which is what
    // makes the ring order match the wire order.
    flushConceal(ad.timestamp);
    rememberDecoded(l, r, frames);
    if (shared) writeShared(l, r, frames);
    else postSamples(l, r, frames);
}

// A decoder that errors on every packet must not be rebuilt forever: past this the page is
// told, so it can drop to the browser's own Opus track instead of playing silence.
const MAX_DECODER_RESETS = 3;
let decoderResets = 0;
const MAX_COPY_FAILURES = 4;
let copyFailures = 0;

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
// A redundant copy that filled a real hole, and one that arrived for a packet we already had.
// The second number is the cost of the feature and the first is what it bought.
let redundantUsed = 0;
let redundantDiscarded = 0;
let lastDecodedL = null;
let lastDecodedR = null;
let lastDecodedFrames = 0;

// Concealment used to be written inline, from the packet-arrival task. Real samples only reach
// the ring from the decoder's output callback, which is a *separate* task, so under load — three
// 5 ms packets queued behind one 15 ms stall — the concealment for a lost packet landed in the
// ring before the output of the packet it follows. That plays two 5 ms blocks in the wrong
// order, precisely in the conditions concealment exists for.
//
// So it is queued against the capture timestamp of the packet it must precede, and emitted from
// that packet's own output task. A timestamp whose decode never produces an output would leak an
// entry, so the map is bounded and drops oldest-first.
const pendingConceal = new Map();
const MAX_PENDING_CONCEAL = 16;
function queueConceal(beforeTimestampUs, n) {
    if (!lastDecodedFrames || n <= 0) return;
    if (pendingConceal.size >= MAX_PENDING_CONCEAL) {
        const oldest = pendingConceal.keys().next();
        if (!oldest.done) pendingConceal.delete(oldest.value);
    }
    pendingConceal.set(beforeTimestampUs, (pendingConceal.get(beforeTimestampUs) || 0) + n);
}
function flushConceal(timestampUs) {
    const n = pendingConceal.get(timestampUs);
    if (!n) return;
    pendingConceal.delete(timestampUs);
    concealPackets(n);
}

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

function submit(buf, off, len, timestampUs) {
    if (len <= 0) return;
    if (!decoder || decoder.state !== 'configured') return;
    try {
        decoder.decode(new EncodedAudioChunk({
            type: 'key',
            timestamp: timestampUs, // micro-s
            data: new Uint8Array(buf, off, len),
        }));
    } catch (e) {
        console.warn('[audio-worker] decode failed:', e);
    }
}

function handlePacket(buf) {
    if (buf.byteLength < HDR_BYTES) return;
    const dv = new DataView(buf);
    const seq = dv.getUint32(0, true);
    const captureNs = readU64(dv, 4);
    const flags = dv.getUint8(12);

    let primaryOff = HDR_BYTES;
    let primaryLen = buf.byteLength - HDR_BYTES;
    let prevOff = 0;
    let prevLen = 0;
    if (flags & FLAG_REDUNDANT) {
        if (buf.byteLength < HDR_BYTES + REDUNDANCY_PREFIX_LEN) return;
        primaryLen = dv.getUint16(HDR_BYTES, true);
        primaryOff = HDR_BYTES + REDUNDANCY_PREFIX_LEN;
        if (buf.byteLength - primaryOff < primaryLen) return;
        prevOff = primaryOff + primaryLen;
        prevLen = buf.byteLength - prevOff;
    }

    let gap = 0;
    if (lastSeq !== null) {
        if (seq === lastSeq) return; // duplicate
        if (((seq - lastSeq) >>> 0) >= 0x80000000) return; // older --> late
        gap = ((seq - lastSeq) >>> 0) - 1;
    }
    const discontinuity = !!(flags & FLAG_DISCONTINUITY);
    const captureUs = Math.round(captureNs / 1000);

    // The trailing copy is the payload for `seq - 1` and nothing else, so it is worth
    // decoding only when that packet never arrived. Otherwise it is a duplicate of audio
    // already in the ring and playing it would double the frame, not repair anything.
    let recovered = false;
    if (prevLen > 0 && !discontinuity) {
        if (gap > 0) {
            // The copy reaches back exactly one frame; anything older is still a hole, and it
            // belongs in front of the copy rather than in front of this packet.
            if (gap > 1) queueConceal(captureUs - FRAME_MS * 1000, gap - 1);
            submit(buf, prevOff, prevLen, captureUs - FRAME_MS * 1000);
            redundantUsed++;
            recovered = true;
        } else {
            redundantDiscarded++;
        }
    }
    // The host is telling us the capture itself broke, so there is nothing to bridge: let
    // the worklet resynchronise instead of pretending the missing audio existed.
    if (!recovered && gap > 0 && !discontinuity) queueConceal(captureUs, gap);

    lastSeq = seq;
    if (discontinuity) {
        lastDecodedFrames = 0;
        // Rides the same channel as the samples it sits between, or it could overtake them.
        postToWorklet({ type: 'discontinuity' });
    }
    submit(buf, primaryOff, primaryLen, captureUs);
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
        pendingConceal.clear();
        concealed = 0;
        redundantUsed = 0;
        redundantDiscarded = 0;
        decoderResets = 0;
        copyFailures = 0;
        // Re-initialising must not leave the previous decoder open: the mode handshake below
        // can send a second `init` in the same session.
        try { if (decoder && decoder.state !== 'closed') decoder.close(); } catch (_) {}
        decoder = null;
        lastDecodedFrames = 0;
        clearShared();
        if (d.sab) setupShared(d.sab, d.capFrames);
        // The reader validates the same buffer independently, and if the two ever disagreed
        // the writer would fill a ring nobody reads while the reader primed forever on a write
        // position nobody advances — permanent silence with both halves believing they work.
        // Say which mode this side actually ended up in.
        self.postMessage({ type: 'mode', shared: shared });
        makeDecoder();
    } else if (d.type === 'clockoffset') {
        if (typeof d.offsetMs === 'number' && isFinite(d.offsetMs)) {
            if (hostOffsetMs === 0) preEnqueueEmaMs = null;
            hostOffsetMs = d.offsetMs;
        }
    } else if (d.type === 'ringport' && d.port) {
        // The page only sends this once the worklet has confirmed it took the other end, so
        // from here the samples go straight across and the main thread is out of the path.
        samplePort = d.port;
    }
};

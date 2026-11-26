const SR = 48000;
const SILENCE_PEAK = 0.0035;

const CTRL_INTS = 16;
const CTRL_BYTES = CTRL_INTS * 4;
const W = 0;
const R = 1;
const OVERRUNS = 2;

const DEFAULT_CAP_FRAMES = 65536;

// Planar audio ring ABI, reader side. Hand-duplicated in audio.js (which owns the allocation
// size) and audio-worker.js (the writer); change all three together.
//
//   [0 .. 64)                        control block, 16 Int32 (W, R, OVERRUNS)
//   [64 .. 64 + cap*4)               left channel,  Float32
//   [64 + cap*4 .. 64 + cap*8)       right channel, Float32
//
// Positions are absolute uint32 frame counters indexed with `pos & (cap-1)`, so the capacity
// must be a power of two.
class AudioJitterProcessor extends AudioWorkletProcessor {
    constructor(options) {
        super();
        const o = (options && options.processorOptions) || {};
        this.capFrames = o.capacityFrames || DEFAULT_CAP_FRAMES;
        if ((this.capFrames & (this.capFrames - 1)) !== 0) {
            throw new Error('audio ring capacity must be a power of two, got ' + this.capFrames);
        }
        this.mask = this.capFrames - 1;

        this.shared = false;
        if (o.sab) {
            const need = CTRL_BYTES + this.capFrames * 8;
            if (o.sab.byteLength < need) {
                throw new Error('audio ring is ' + o.sab.byteLength + ' bytes, needs ' + need);
            }
            try {
                this.ctrl = new Int32Array(o.sab, 0, CTRL_INTS);
                this.ringL = new Float32Array(o.sab, CTRL_BYTES, this.capFrames);
                this.ringR = new Float32Array(o.sab, CTRL_BYTES + this.capFrames * 4, this.capFrames);
                this.shared = true;
            } catch (_) {
                this.shared = false;
            }
        }
        if (!this.shared) {
            this.ctrl = null;
            this.ringL = new Float32Array(this.capFrames);
            this.ringR = new Float32Array(this.capFrames);
        }

        this.readPos = this.shared ? (Atomics.load(this.ctrl, W) >>> 0) : 0;
        this.writePos = this.readPos;

        this.targetFrames = Math.round(SR * 0.012);
        this.minTarget = Math.round(SR * 0.010);
        this.maxTarget = Math.round(SR * 0.040);
        this.slackFrames = Math.round(SR * 0.005);
        this.hardSlackFrames = Math.round(SR * 0.25);
        this.commandedTargetFrames = 0;
        this.corrections = 0;
        this.priming = true;
        this.underruns = 0;
        this.overruns = 0;
        this.stableBlocks = 0;
        this.blockCount = 0;
        // re-priming outputs silence until the ring refills to the target, so on a single
        // missed quantum one lost 5 ms packet becomes 10-40 ms of silence: a real stall is
        // several starved blocks in a row
        this.starvedBlocks = 0;
        this.REPRIME_AFTER_STARVED = 4;
        // the last quantum played, repeated with a fade as concealment: far less noticeable
        // than a square-edged silence
        this.concealL = new Float32Array(128);
        this.concealR = new Float32Array(128);
        this.concealFrames = 0;
        this.concealGain = 1;
        this.concealed = 0;
        this.port.onmessage = (e) => this.onMessage(e.data);
    }

    onMessage(d) {
        if (!d) return;
        if (d.type === 'samples' && d.l && d.r) {
            this.enqueue(d.l, d.r);
        } else if (d.type === 'target' && typeof d.targetMs === 'number') {
            const frames = Math.round((d.targetMs / 1000) * SR);
            this.commandedTargetFrames = Math.max(0, Math.min(this.capFrames - 1, frames));
        } else if (d.type === 'discontinuity') {
            // the stream itself broke (a device change, a dropped broadcast slot): nothing
            // in the ring bridges that, so correct now rather than at a silence boundary
            this.concealFrames = 0;
            this.priming = true;
            this.corrections++;
        } else if (d.type === 'mode') {
            // the writer validated the same buffer on its own side: if it ended up in a
            // different mode from this one, every packet goes somewhere nobody reads
            const writerShared = !!d.shared;
            if (writerShared === this.shared) return;
            if (this.shared) {
                // the writer could not map the ring, so nothing advances the write position
                // read here: fall back to the private ring it will postMessage into
                this.shared = false;
                this.ctrl = null;
                this.ringL = new Float32Array(this.capFrames);
                this.ringR = new Float32Array(this.capFrames);
                this.readPos = 0;
                this.writePos = 0;
                this.concealFrames = 0;
                this.starvedBlocks = 0;
                this.priming = true;
            } else {
                // the writer mapped it and this side could not: only the page can undo that
                this.port.postMessage({ type: 'mode-mismatch' });
            }
        } else if (d.type === 'ringport' && d.port) {
            // The decode worker's end of a MessageChannel: taking it removes the main thread
            // from the sample path on the private-ring transport (worker -> page -> here, 200
            // times a second). The acknowledgement is what releases the far end to the worker:
            // on an engine that will not transfer a port into this scope nothing arrives, no
            // acknowledgement goes back, and the page relay carries on.
            this.samplePort = d.port;
            this.samplePort.onmessage = (e) => this.onMessage(e.data);
            this.port.postMessage({ type: 'ringport-ok' });
        } else if (d.type === 'reset') {
            this.readPos = this.shared ? (Atomics.load(this.ctrl, W) >>> 0) : this.writePos;
            if (this.shared) Atomics.store(this.ctrl, R, this.readPos | 0);
            this.concealFrames = 0;
            this.starvedBlocks = 0;
            this.priming = true;
        }
    }

    available() {
        const w = this.shared ? (Atomics.load(this.ctrl, W) >>> 0) : this.writePos;
        return (w - this.readPos) >>> 0;
    }

    advanceRead(n) {
        this.readPos = (this.readPos + n) >>> 0;
        if (this.shared) Atomics.store(this.ctrl, R, this.readPos | 0);
    }

    enqueue(l, r) {
        const framesIn = l.length;
        if (framesIn <= 0) return;
        const free = this.capFrames - this.available();
        let n = framesIn;
        if (n > free) {
            this.overruns++;
            n = free;
            if (n <= 0) return;
        }
        const start = this.writePos & this.mask;
        const first = Math.min(n, this.capFrames - start);
        this.ringL.set(l.subarray(0, first), start);
        this.ringR.set(r.subarray(0, first), start);
        if (first < n) {
            this.ringL.set(l.subarray(first, n), 0);
            this.ringR.set(r.subarray(first, n), 0);
        }
        this.writePos = (this.writePos + n) >>> 0;
    }

    peekPeak(n) {
        const m = Math.min(n, this.available());
        let p = 0;
        let i = this.readPos & this.mask;
        for (let k = 0; k < m; k++) {
            const a0 = this.ringL[i] < 0 ? -this.ringL[i] : this.ringL[i];
            if (a0 > p) p = a0;
            const a1 = this.ringR[i] < 0 ? -this.ringR[i] : this.ringR[i];
            if (a1 > p) p = a1;
            i = (i + 1) & this.mask;
        }
        return p;
    }

    dequeueInto(outL, outR, n) {
        const start = this.readPos & this.mask;
        const first = Math.min(n, this.capFrames - start);
        outL.set(this.ringL.subarray(start, start + first), 0);
        if (outR) outR.set(this.ringR.subarray(start, start + first), 0);
        if (first < n) {
            outL.set(this.ringL.subarray(0, n - first), first);
            if (outR) outR.set(this.ringR.subarray(0, n - first), first);
        }
        this.advanceRead(n);
    }

    process(_inputs, outputs, _params) {
        const out = outputs[0];
        if (!out || out.length < 1) return true;
        const outL = out[0];
        const outR = out.length > 1 ? out[1] : null;
        const need = outL.length; // always 128

        this.blockCount++;

        const commanded = this.commandedTargetFrames > 0;
        const effTarget = commanded ? this.commandedTargetFrames : this.targetFrames;
        let avail = this.available();

        if (avail > effTarget + this.hardSlackFrames) {
            this.advanceRead(avail - effTarget);
            avail = effTarget;
            this.corrections++;
        }

        if (this.priming) {
            if (avail >= effTarget) {
                this.priming = false;
            } else {
                outL.fill(0);
                if (outR) outR.fill(0);
                this.maybePostStats();
                return true;
            }
        }

        if (commanded) {
            const err = avail - effTarget;
            if (err > this.slackFrames && this.peekPeak(need) < SILENCE_PEAK) {
                const drop = Math.min(err - this.slackFrames, this.slackFrames);
                this.advanceRead(drop);
                avail -= drop;
                this.corrections++;
            // `avail > 0` keeps a starved ring out of this branch, which exists to grow the
            // buffer at a silence boundary. A commanded target is always at least
            // MIN_TARGET_MS against 5 ms of slack, so without it an empty ring returns a
            // silent quantum here and never reaches conceal(), underruns++ or the re-prime
            // hysteresis below.
            } else if (err < -this.slackFrames && avail > 0 && this.peekPeak(need) < SILENCE_PEAK) {
                outL.fill(0);
                if (outR) outR.fill(0);
                this.corrections++;
                this.maybePostStats();
                return true;
            }
        }

        if (avail >= need) {
            this.dequeueInto(outL, outR, need);
            this.rememberQuantum(outL, outR, need);
            this.starvedBlocks = 0;
            this.stableBlocks++;
            if (!commanded && this.stableBlocks > 750 && this.targetFrames > this.minTarget) {
                this.targetFrames = Math.max(this.minTarget, this.targetFrames - Math.round(SR * 0.001));
                this.stableBlocks = 0;
            }
        } else {
            if (avail > 0) {
                this.dequeueInto(outL, outR, avail);
                this.rememberQuantum(outL, outR, avail);
            }
            this.conceal(outL, outR, avail, need);
            this.underruns++;
            this.starvedBlocks++;
            this.stableBlocks = 0;
            if (!commanded && this.targetFrames < this.maxTarget) {
                this.targetFrames = Math.min(this.maxTarget, this.targetFrames + Math.round(SR * 0.003));
            }
            // only a sustained stall is worth the silence a re-prime costs
            if (this.starvedBlocks >= this.REPRIME_AFTER_STARVED) this.priming = true;
        }

        this.maybePostStats();
        return true;
    }

    rememberQuantum(outL, outR, n) {
        if (n <= 0) return;
        const m = Math.min(n, this.concealL.length);
        this.concealL.set(outL.subarray(n - m, n), 0);
        if (outR) this.concealR.set(outR.subarray(n - m, n), 0);
        else this.concealR.set(this.concealL.subarray(0, m), 0);
        this.concealFrames = m;
        this.concealGain = 1;
    }

    // fill [from, need) with a fading repeat of the last audio played: the gain decays to
    // nothing, so a real stall still ends in silence, just without a click
    conceal(outL, outR, from, need) {
        if (this.concealFrames <= 0) {
            outL.fill(0, from);
            if (outR) outR.fill(0, from);
            return;
        }
        this.concealed++;
        let g = this.concealGain;
        for (let i = from; i < need; i++) {
            const s = i % this.concealFrames;
            outL[i] = this.concealL[s] * g;
            if (outR) outR[i] = this.concealR[s] * g;
            g *= 0.9993; // ~1/e over three quanta
        }
        this.concealGain = g < 0.001 ? 0 : g;
        if (this.concealGain === 0) this.concealFrames = 0;
    }

    maybePostStats() {
        if (this.blockCount % 96 !== 0) return;
        const effTarget = this.commandedTargetFrames > 0 ? this.commandedTargetFrames : this.targetFrames;
        const overruns = this.shared ? Atomics.load(this.ctrl, OVERRUNS) : this.overruns;
        this.port.postMessage({
            type: 'stats',
            depthMs: (this.available() / SR) * 1000,
            targetMs: (effTarget / SR) * 1000,
            underruns: this.underruns,
            overruns: overruns,
            corrections: this.corrections,
            concealed: this.concealed,
            shared: this.shared,
        });
    }
}

registerProcessor('audio-jitter', AudioJitterProcessor);

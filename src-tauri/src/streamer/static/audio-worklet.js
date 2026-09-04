const SR = 48000;
const SILENCE_PEAK = 0.0035;

const CTRL_INTS = 16;
const CTRL_BYTES = CTRL_INTS * 4;
const W = 0;
const R = 1;
const OVERRUNS = 2;

const DEFAULT_CAP_FRAMES = 65536;

class AudioJitterProcessor extends AudioWorkletProcessor {
    constructor(options) {
        super();
        const o = (options && options.processorOptions) || {};
        this.capFrames = o.capacityFrames || DEFAULT_CAP_FRAMES;
        this.mask = this.capFrames - 1;

        this.shared = false;
        if (o.sab) {
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
        this.port.onmessage = (e) => this.onMessage(e.data);
    }

    onMessage(d) {
        if (!d) return;
        if (d.type === 'samples' && d.l && d.r) {
            this.enqueue(d.l, d.r);
        } else if (d.type === 'target' && typeof d.targetMs === 'number') {
            const frames = Math.round((d.targetMs / 1000) * SR);
            this.commandedTargetFrames = Math.max(0, Math.min(this.capFrames - 1, frames));
        } else if (d.type === 'reset') {
            this.readPos = this.shared ? (Atomics.load(this.ctrl, W) >>> 0) : this.writePos;
            if (this.shared) Atomics.store(this.ctrl, R, this.readPos | 0);
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
            } else if (err < -this.slackFrames && (avail === 0 || this.peekPeak(need) < SILENCE_PEAK)) {
                outL.fill(0);
                if (outR) outR.fill(0);
                this.corrections++;
                this.maybePostStats();
                return true;
            }
        }

        if (avail >= need) {
            this.dequeueInto(outL, outR, need);
            this.stableBlocks++;
            if (!commanded && this.stableBlocks > 750 && this.targetFrames > this.minTarget) {
                this.targetFrames = Math.max(this.minTarget, this.targetFrames - Math.round(SR * 0.001));
                this.stableBlocks = 0;
            }
        } else {
            if (avail > 0) this.dequeueInto(outL, outR, avail);
            outL.fill(0, avail);
            if (outR) outR.fill(0, avail);
            this.underruns++;
            this.stableBlocks = 0;
            if (!commanded && this.targetFrames < this.maxTarget) {
                this.targetFrames = Math.min(this.maxTarget, this.targetFrames + Math.round(SR * 0.003));
            }
            this.priming = true; // re-prime to the target
        }

        this.maybePostStats();
        return true;
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
            shared: this.shared,
        });
    }
}

registerProcessor('audio-jitter', AudioJitterProcessor);

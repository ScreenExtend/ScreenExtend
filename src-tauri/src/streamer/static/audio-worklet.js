const SR = 48000;
const CH = 2;
const SILENCE_PEAK = 0.0035;

class AudioJitterProcessor extends AudioWorkletProcessor {
    constructor(options) {
        super();
        const o = (options && options.processorOptions) || {};
        this.capFrames = o.capacityFrames || SR;
        this.buf = new Float32Array(this.capFrames * CH);
        this.readFrame = 0;
        this.availFrames = 0;
        this.targetFrames = Math.round(SR * 0.012);
        this.minTarget = Math.round(SR * 0.010);
        this.maxTarget = Math.round(SR * 0.040);
        this.commandedTargetFrames = 0;
        this.corrections = 0;
        this.priming = true;
        this.underruns = 0;
        this.overruns = 0;
        this.recentUnderruns = 0;
        this.stableBlocks = 0;
        this.blockCount = 0;
        this.port.onmessage = (e) => this.onMessage(e.data);
    }

    onMessage(d) {
        if (d && d.type === 'samples' && d.data) {
            this.enqueue(d.data);
        } else if (d && d.type === 'target' && typeof d.targetMs === 'number') {
            const frames = Math.round((d.targetMs / 1000) * SR);
            this.commandedTargetFrames = Math.max(0, Math.min(this.capFrames - 1, frames));
        } else if (d && d.type === 'reset') {
            this.readFrame = 0;
            this.availFrames = 0;
            this.priming = true;
        }
    }

    peekPeak(n) {
        const m = Math.min(n, this.availFrames);
        let p = 0;
        let r = this.readFrame;
        for (let i = 0; i < m; i++) {
            const b = r * CH;
            const a0 = this.buf[b] < 0 ? -this.buf[b] : this.buf[b];
            if (a0 > p) p = a0;
            const a1 = this.buf[b + 1] < 0 ? -this.buf[b + 1] : this.buf[b + 1];
            if (a1 > p) p = a1;
            r++;
            if (r === this.capFrames) r = 0;
        }
        return p;
    }

    enqueue(interleaved) {
        const framesIn = (interleaved.length / CH) | 0;
        if (framesIn <= 0) return;

        const free = this.capFrames - this.availFrames;
        if (framesIn > free) {
            const drop = framesIn - free;
            this.readFrame = (this.readFrame + drop) % this.capFrames;
            this.availFrames -= drop;
            this.overruns++;
        }

        let writeFrame = (this.readFrame + this.availFrames) % this.capFrames;
        for (let i = 0; i < framesIn; i++) {
            const w = writeFrame * CH;
            this.buf[w] = interleaved[i * CH];
            this.buf[w + 1] = interleaved[i * CH + 1];
            writeFrame = writeFrame + 1;
            if (writeFrame === this.capFrames) writeFrame = 0;
        }
        this.availFrames += framesIn;
    }

    dequeueInto(outL, outR, n) {
        for (let i = 0; i < n; i++) {
            const r = this.readFrame * CH;
            outL[i] = this.buf[r];
            outR[i] = this.buf[r + 1];
            this.readFrame = this.readFrame + 1;
            if (this.readFrame === this.capFrames) this.readFrame = 0;
        }
        this.availFrames -= n;
    }

    process(_inputs, outputs, _params) {
        const out = outputs[0];
        if (!out || out.length < 1) return true;
        const outL = out[0];
        const outR = out.length > 1 ? out[1] : out[0];
        const need = outL.length; // always 128

        this.blockCount++;

        const commanded = this.commandedTargetFrames > 0;
        const effTarget = commanded ? this.commandedTargetFrames : this.targetFrames;

        if (this.priming) {
            if (this.availFrames >= effTarget) {
                this.priming = false;
            } else {
                outL.fill(0);
                if (outR !== outL) outR.fill(0);
                this.maybePostStats();
                return true;
            }
        }

        if (commanded) {
            const err = this.availFrames - effTarget;
            const slack = Math.round(SR * 0.005);
            if (err > slack && this.peekPeak(need) < SILENCE_PEAK) {
                const drop = Math.min(err - slack, Math.round(SR * 0.005));
                this.readFrame = (this.readFrame + drop) % this.capFrames;
                this.availFrames -= drop;
                this.corrections++;
            } else if (err < -slack && (this.availFrames === 0 || this.peekPeak(need) < SILENCE_PEAK)) {
                outL.fill(0);
                if (outR !== outL) outR.fill(0);
                this.corrections++;
                this.maybePostStats();
                return true;
            }
        }

        if (this.availFrames >= need) {
            this.dequeueInto(outL, outR, need);
            this.stableBlocks++;
            if (!commanded && this.stableBlocks > 750 && this.targetFrames > this.minTarget) {
                this.targetFrames = Math.max(this.minTarget, this.targetFrames - Math.round(SR * 0.001));
                this.stableBlocks = 0;
            }
        } else {
            const have = this.availFrames;
            if (have > 0) this.dequeueInto(outL, outR, have);
            for (let i = have; i < need; i++) {
                outL[i] = 0;
                if (outR !== outL) outR[i] = 0;
            }
            this.underruns++;
            this.recentUnderruns++;
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
        this.port.postMessage({
            type: 'stats',
            depthMs: (this.availFrames / SR) * 1000,
            targetMs: (effTarget / SR) * 1000,
            underruns: this.underruns,
            overruns: this.overruns,
            corrections: this.corrections,
        });
        this.recentUnderruns = 0;
    }
}

registerProcessor('audio-jitter', AudioJitterProcessor);

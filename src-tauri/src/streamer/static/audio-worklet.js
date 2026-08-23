// audio-worklet.js — the playback consumer with OUR jitter buffer (PRD §6.2), not NetEQ.
//
// Decoded interleaved-stereo f32 samples arrive from the main thread (postMessage ring — the
// v1 transport, since the fast HTTPS/SAB path isn't landed yet; see AUDIO_NOTES §3.6). We keep
// a small circular buffer whose depth WE control (target 10–15 ms), grow it on repeated
// underruns and shrink it after a stable period, emit silence (never stall) on underrun, and
// drop the oldest on overrun. Counters are posted back to the main thread for the overlay (§9).

const SR = 48000;
const CH = 2;

// Below this peak amplitude a stretch counts as silence, where we may drop/insert samples to
// correct A/V drift without an audible cut (PRD §6.5). ~ -49 dBFS.
const SILENCE_PEAK = 0.0035;

class AudioJitterProcessor extends AudioWorkletProcessor {
    constructor(options) {
        super();
        const o = (options && options.processorOptions) || {};

        // Circular buffer of interleaved stereo samples. ~1s is plenty of headroom.
        this.capFrames = o.capacityFrames || SR;
        this.buf = new Float32Array(this.capFrames * CH);
        this.readFrame = 0;
        this.availFrames = 0;

        // Adaptive target depth, in frames. Start at 12 ms; clamp 10–40 ms. Used until the main
        // thread commands a target for A/V sync, after which `commandedTargetFrames` takes over.
        this.targetFrames = Math.round(SR * 0.012);
        this.minTarget = Math.round(SR * 0.010);
        this.maxTarget = Math.round(SR * 0.040);

        // A/V-sync commanded depth (0 = not set → adaptive). When set, we hold this depth and
        // correct drift toward it only across silent stretches (§6.5).
        this.commandedTargetFrames = 0;
        this.corrections = 0;

        this.priming = true; // hold output until we've buffered the target depth
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
            // Bound to the buffer's headroom; grow re-primes, shrink corrects at silence.
            const frames = Math.round((d.targetMs / 1000) * SR);
            this.commandedTargetFrames = Math.max(0, Math.min(this.capFrames - 1, frames));
        } else if (d && d.type === 'reset') {
            this.readFrame = 0;
            this.availFrames = 0;
            this.priming = true;
        }
    }

    // Max abs amplitude of the next `n` buffered frames without consuming them (silence probe).
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

        // Overrun: if it won't fit, drop the oldest frames to make room (never grow unbounded).
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

        // Effective target: the A/V-sync command overrides the adaptive estimate when present.
        const commanded = this.commandedTargetFrames > 0;
        const effTarget = commanded ? this.commandedTargetFrames : this.targetFrames;

        // Prime: stay silent until we have the target depth buffered, so playback starts smooth.
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

        // A/V-sync drift correction — only across silent stretches, never cutting a tone (§6.5).
        if (commanded) {
            const err = this.availFrames - effTarget; // >0 too deep (audio late), <0 too shallow
            const slack = Math.round(SR * 0.005); // 5 ms deadband
            if (err > slack && this.peekPeak(need) < SILENCE_PEAK) {
                // Too much latency: drop a little silence to catch up (≤5 ms/block).
                const drop = Math.min(err - slack, Math.round(SR * 0.005));
                this.readFrame = (this.readFrame + drop) % this.capFrames;
                this.availFrames -= drop;
                this.corrections++;
            } else if (err < -slack && (this.availFrames === 0 || this.peekPeak(need) < SILENCE_PEAK)) {
                // Too little latency: insert a silent block to build depth without a gap in sound.
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
            // Adaptive shrink (only when not sync-commanded): shed latency after ~2s stable.
            if (!commanded && this.stableBlocks > 750 && this.targetFrames > this.minTarget) {
                this.targetFrames = Math.max(this.minTarget, this.targetFrames - Math.round(SR * 0.001));
                this.stableBlocks = 0;
            }
        } else {
            // Underrun: emit what we have, pad with silence. Grow the adaptive target when we're
            // driving it; when sync-commanded the depth is fixed, so just re-prime to it.
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
        // ~ every 250 ms (128-frame blocks at 48 kHz ≈ 2.67 ms).
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

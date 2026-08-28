(function () {
    'use strict';

    const SR = 48000;
    const CH = 2;
    const MIN_TARGET_MS = 10;
    const MAX_TARGET_MS = 400;
    const P_MS = 0x100000000 / 90;

    function detectCapabilities() {
        const AC = window.AudioContext || window.webkitAudioContext;
        const worklet = !!(AC && AC.prototype && 'audioWorklet' in AC.prototype);
        const webcodecsOpus =
            typeof window.AudioDecoder === 'function' &&
            typeof window.EncodedAudioChunk === 'function' &&
            typeof window.AudioData === 'function';
        const sab = self.crossOriginIsolated === true && typeof SharedArrayBuffer !== 'undefined';
        return { webcodecsOpus, worklet, sab };
    }

    const state = {
        ctx: null,
        node: null,
        gain: null,
        decoder: null,
        dc: null,
        audioEl: null,
        path: null, // 'webcodecs' | 'netEQ' | null
        muted: false,
        prepared: false,
        firstChunk: false,
        // A/V sync running state
        videoDelayMs: null,     // from the video worker (EMA of display lag)
        preEnqueueEmaMs: null,  // audio capture→enqueue latency, smoothed
        lastTargetMs: null,     // last depth commanded to the worklet
        lastTargetAt: 0,
        residualOffsetMs: 0,    // unachievable sync error after clamping (diagnostics)
        lastOffsetLogAt: 0,
    };

    function outputLatencyMs() {
        const c = state.ctx;
        if (!c) return 0;
        const base = typeof c.baseLatency === 'number' ? c.baseLatency : 0;
        const out = typeof c.outputLatency === 'number' ? c.outputLatency : 0;
        return (base + out) * 1000;
    }

    function maybeUpdateTarget() {
        if (state.path !== 'webcodecs' || !state.node) return;
        if (state.videoDelayMs === null || state.preEnqueueEmaMs === null) return;

        let raw = state.videoDelayMs - state.preEnqueueEmaMs - outputLatencyMs();
        raw = ((raw % P_MS) + P_MS) % P_MS;
        if (raw > P_MS / 2) raw -= P_MS;

        const target = Math.max(MIN_TARGET_MS, Math.min(MAX_TARGET_MS, raw));
        state.residualOffsetMs = raw - target; // 0 when sync is achievable within bounds

        const now = (typeof performance !== 'undefined' ? performance.now() : 0);
        if (now - state.lastOffsetLogAt > 2000) {
            state.lastOffsetLogAt = now;
            console.log('[audio] A/V sync: target=' + target.toFixed(1) + 'ms residual=' +
                state.residualOffsetMs.toFixed(1) + 'ms (videoDelay=' + state.videoDelayMs.toFixed(1) +
                ' preEnqueue=' + state.preEnqueueEmaMs.toFixed(1) + ')');
        }
        if (state.lastTargetMs !== null && Math.abs(target - state.lastTargetMs) < 3 &&
            now - state.lastTargetAt < 300) {
            return;
        }
        state.lastTargetMs = target;
        state.lastTargetAt = now;
        state.node.port.postMessage({ type: 'target', targetMs: target });
    }

    function setVideoDelay(ms) {
        if (typeof ms !== 'number' || !isFinite(ms)) return;
        state.videoDelayMs = ms;
        maybeUpdateTarget();
    }

    function setMuted(m) {
        state.muted = m;
        if (state.gain) state.gain.gain.value = m ? 0 : 1;
        if (state.audioEl) state.audioEl.muted = m;
    }

    async function prepare() {
        if (state.prepared) return true;
        const AC = window.AudioContext || window.webkitAudioContext;
        if (!AC) return false;
        try {
            state.ctx = new AC({ latencyHint: 'interactive', sampleRate: SR });
            await state.ctx.audioWorklet.addModule('/audio-worklet.js');
            state.node = new AudioWorkletNode(state.ctx, 'audio-jitter', {
                numberOfInputs: 0,
                numberOfOutputs: 1,
                outputChannelCount: [CH],
            });
            state.gain = state.ctx.createGain();
            state.gain.gain.value = state.muted ? 0 : 1;
            state.node.connect(state.gain).connect(state.ctx.destination);
            state.prepared = true;
            return true;
        } catch (e) {
            console.warn('[audio] prepare failed:', e);
            return false;
        }
    }

    async function resume() {
        if (state.ctx && state.ctx.state === 'suspended') {
            try { await state.ctx.resume(); } catch (_) {}
        }
    }

    function makeDecoder() {
        state.decoder = new AudioDecoder({
            output: onDecoded,
            error: (e) => console.warn('[audio] decoder error:', e),
        });
        state.decoder.configure({ codec: 'opus', sampleRate: SR, numberOfChannels: CH });
    }

    function onDecoded(audioData) {
        const frames = audioData.numberOfFrames;
        const chs = audioData.numberOfChannels;
        const captureHostMs = audioData.timestamp / 1000;
        const inter = new Float32Array(frames * CH);
        const tmp = new Float32Array(frames);
        try {
            audioData.copyTo(tmp, { planeIndex: 0, format: 'f32-planar' });
            for (let i = 0; i < frames; i++) inter[i * CH] = tmp[i];
            if (chs >= 2) {
                audioData.copyTo(tmp, { planeIndex: 1, format: 'f32-planar' });
                for (let i = 0; i < frames; i++) inter[i * CH + 1] = tmp[i];
            } else {
                for (let i = 0; i < frames; i++) inter[i * CH + 1] = inter[i * CH];
            }
        } catch (e) {
            audioData.close();
            return;
        }
        audioData.close();

        const enqueueAbsMs = performance.timeOrigin + performance.now();
        const preEnq = enqueueAbsMs - captureHostMs;
        state.preEnqueueEmaMs = (state.preEnqueueEmaMs === null)
            ? preEnq : state.preEnqueueEmaMs * 0.9 + preEnq * 0.1;
        maybeUpdateTarget();

        if (state.node) state.node.port.postMessage({ type: 'samples', data: inter }, [inter.buffer]);
    }

    async function attachDataChannel(dc) {
        if (!(await prepare())) {
            console.warn('[audio] cannot prepare WebCodecs path');
            return;
        }
        makeDecoder();
        state.dc = dc;
        state.lastSeq = null;
        dc.binaryType = 'arraybuffer';
        dc.onmessage = (ev) => {
            const buf = ev.data;
            if (!(buf instanceof ArrayBuffer) || buf.byteLength < 13) return;
            const dv = new DataView(buf);
            const seq = dv.getUint32(0, true);
            const captureNs = dv.getBigUint64(4, true);
            if (state.lastSeq !== null) {
                if (seq === state.lastSeq) return; // duplicate
                if (((seq - state.lastSeq) >>> 0) >= 0x80000000) return; // older --> late
            }
            state.lastSeq = seq;
            const opus = new Uint8Array(buf, 13);
            if (opus.byteLength === 0) return;
            try {
                const chunk = new EncodedAudioChunk({
                    type: 'key',
                    timestamp: Number(captureNs / 1000n), // mew-s
                    data: opus,
                });
                if (state.decoder && state.decoder.state === 'configured') state.decoder.decode(chunk);
            } catch (e) {
                console.warn('[audio] decode failed:', e);
            }
        };
        state.path = 'webcodecs';
        state.firstChunk = true;
        setMuted(false); // default unmuted when the host has enabled audio
        console.log('[audio] fast path active: Opus over DataChannel → WebCodecs');
    }

    function attachFallbackStream(stream) {
        if (!state.audioEl) {
            const a = document.createElement('audio');
            a.autoplay = true;
            a.playsInline = true;
            a.style.display = 'none';
            document.body.appendChild(a);
            state.audioEl = a;
        }
        state.audioEl.srcObject = stream;
        state.audioEl.muted = state.muted;
        const p = state.audioEl.play();
        if (p && p.catch) p.catch(() => {});
        state.path = 'netEQ';
        setMuted(false);
        console.log('[audio] fallback path active: standard Opus track (NetEQ)');
    }

    function activePath() {
        return state.path;
    }

    function teardown() {
        try { if (state.decoder && state.decoder.state !== 'closed') state.decoder.close(); } catch (_) {}
        state.decoder = null;
        if (state.dc) { try { state.dc.onmessage = null; } catch (_) {} state.dc = null; }
        if (state.audioEl) { try { state.audioEl.srcObject = null; } catch (_) {} }
        if (state.node) { try { state.node.port.postMessage({ type: 'reset' }); } catch (_) {} }
        state.path = null;
        state.preEnqueueEmaMs = null;
        state.lastTargetMs = null;
        state.residualOffsetMs = 0;
    }

    function getSyncInfo() {
        return {
            path: state.path,
            videoDelayMs: state.videoDelayMs,
            preEnqueueMs: state.preEnqueueEmaMs,
            targetMs: state.lastTargetMs,
            residualOffsetMs: state.residualOffsetMs,
        };
    }

    window.SEAudio = {
        detectCapabilities,
        prepare,
        resume,
        attachDataChannel,
        attachFallbackStream,
        setMuted,
        isMuted: () => state.muted,
        setVideoDelay,
        getSyncInfo,
        activePath,
        teardown,
    };
})();

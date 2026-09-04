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

    const SE_LOUDSPEAKER = 'se:loudspeaker';
    const SE_EARPIECE = 'se:earpiece';
    const MIC_PRIME_TIMEOUT_MS = 12000;

    const speakers = {
        inited: false,
        hasEnumerate: false,
        hasElemSink: false,
        hasCtxSink: false,
        hasSelectOut: false,
        hasDeviceChange: false,
        hasAudioSession: false,
        isIOS: false,
        hasEarpiece: false,
        canSink: false,
        canCategory: false,
        supported: false,
        watching: false,
        poll: null,
        firstDeviceChange: true,
        debounceTimer: 0,
        appliedSink: null,
        msd: null,
        sinkEl: null,
        micHold: null,
        micPrime: null,
        micGranted: false,
        primedStream: null,
        wantPrime: null,
        sessionId: '',
        deviceToken: '',
    };

    function detectIOS() {
        try {
            const ua = navigator.userAgent || '';
            const iPad = navigator.platform === 'MacIntel' && (navigator.maxTouchPoints || 0) > 1;
            return /iPad|iPhone|iPod/.test(ua) || iPad;
        } catch (_) { return false; }
    }

    function detectHasEarpiece() {
        try { return /iPhone/.test(navigator.userAgent || ''); } catch (_) { return false; }
    }

    function initSpeakers() {
        if (speakers.inited) return;
        speakers.inited = true;
        try {
            const md = navigator.mediaDevices;
            const AC = window.AudioContext || window.webkitAudioContext;
            speakers.hasEnumerate = !!(md && md.enumerateDevices);
            speakers.hasElemSink = 'setSinkId' in HTMLMediaElement.prototype;
            speakers.hasCtxSink = !!(AC && 'setSinkId' in (AC.prototype || {}));
            speakers.hasSelectOut = !!(md && typeof md.selectAudioOutput === 'function');
            speakers.hasDeviceChange = !!(md && 'ondevicechange' in md);
            speakers.hasAudioSession = 'audioSession' in navigator;
            speakers.isIOS = detectIOS();
            speakers.hasEarpiece = detectHasEarpiece();
            speakers.canSink = speakers.hasEnumerate && (speakers.hasElemSink || speakers.hasCtxSink);
            speakers.canCategory = speakers.hasEarpiece;
            speakers.supported = speakers.canSink || speakers.canCategory;
        } catch (_) {}
        if (speakers.hasAudioSession) {
            try { navigator.audioSession.type = 'playback'; } catch (_) {}
        }
    }

    function micWanted() {
        if (speakers.wantPrime !== null) return speakers.wantPrime;
        return speakers.canSink || (speakers.hasEarpiece && !speakers.hasAudioSession);
    }

    function primeMicPermission() {
        initSpeakers();
        if (speakers.micPrime) return speakers.micPrime;
        if (!speakers.supported || !micWanted()) {
            speakers.micPrime = Promise.resolve(null);
            return speakers.micPrime;
        }
        let req;
        try {
            req = navigator.mediaDevices.getUserMedia({ audio: true });
        } catch (e) {
            req = Promise.reject(e);
        }
        speakers.micPrime = req.then(
            (s) => { speakers.micGranted = true; speakers.primedStream = s; return s; },
            () => { speakers.micGranted = false; return null; }
        );
        return speakers.micPrime;
    }

    function micReady() {
        const p = primeMicPermission();
        return Promise.race([
            p,
            new Promise((r) => setTimeout(() => r(null), MIC_PRIME_TIMEOUT_MS)),
        ]).catch(() => null);
    }

    function releasePrimedStream() {
        if (!speakers.primedStream) return;
        try { speakers.primedStream.getTracks().forEach((t) => t.stop()); } catch (_) {}
        speakers.primedStream = null;
    }

    async function unlockAndEnumerate() {
        initSpeakers();
        if (!speakers.supported) return { supported: false, outputs: [] };
        const primed = primeMicPermission();
        try {
            await primed;
            let enumerated = [];
            if (speakers.canSink) {
                enumerated = await listOutputs();
                if (enumerated.some((d) => d.label)) speakers._lastEnum = enumerated;
                else if (speakers._lastEnum && speakers._lastEnum.length) enumerated = speakers._lastEnum;
            }
            releasePrimedStream();
            const outputs = buildOutputs(enumerated);
            startDeviceWatch();
            speakers._cache = { supported: speakers.supported, outputs };
            return speakers._cache;
        } catch (_) {
            releasePrimedStream();
            speakers._cache = { supported: speakers.supported, outputs: buildOutputs([]) };
            return speakers._cache;
        }
    }

    async function needsMicPermission() {
        initSpeakers();
        speakers.wantPrime = false;
        if (!speakers.canSink && !(speakers.hasEarpiece && !speakers.hasAudioSession)) return false;
        try {
            const p = await navigator.permissions.query({ name: 'microphone' });
            speakers.micGranted = !!(p && p.state === 'granted');
        } catch (_) {}
        if (speakers.canSink) {
            const devs = await listOutputs();
            if (devs.some((d) => d.label)) {
                speakers._lastEnum = devs;
                return false;
            }
        }
        speakers.wantPrime = true;
        return !speakers.micGranted;
    }

    async function listOutputs() {
        try {
            const devs = await navigator.mediaDevices.enumerateDevices();
            return devs
                .filter((d) => d.kind === 'audiooutput')
                .map((d) => ({ id: d.deviceId, label: d.label || '' }));
        } catch (_) {
            return [];
        }
    }

    function buildOutputs(enumerated) {
        const out = [];
        if (speakers.canCategory) {
            out.push({ id: SE_LOUDSPEAKER, label: 'Speaker' });
            out.push({ id: SE_EARPIECE, label: 'Earpiece' });
        }
        if (speakers.canSink) {
            for (const d of enumerated) out.push(d);
        }
        return out;
    }

    function applySession(type) {
        if (!speakers.hasAudioSession) return false;
        try { navigator.audioSession.type = type; return true; } catch (_) { return false; }
    }

    async function acquireMicHold() {
        if (speakers.micHold) return true;
        if (speakers.primedStream) {
            speakers.micHold = speakers.primedStream;
            speakers.primedStream = null;
            return true;
        }
        if (!speakers.micGranted) return false;
        try {
            speakers.micHold = await navigator.mediaDevices.getUserMedia({ audio: true });
            return true;
        } catch (_) { speakers.micHold = null; return false; }
    }

    function releaseMicHold() {
        if (!speakers.micHold) return;
        try { speakers.micHold.getTracks().forEach((t) => t.stop()); } catch (_) {}
        speakers.micHold = null;
    }

    async function revertSink() {
        if (speakers.msd) {
            try { state.gain.disconnect(speakers.msd); } catch (_) {}
            try { if (state.ctx) state.gain.connect(state.ctx.destination); } catch (_) {}
            if (speakers.sinkEl) { try { speakers.sinkEl.pause(); } catch (_) {} speakers.sinkEl.srcObject = null; }
            speakers.msd = null;
        } else if (speakers.hasCtxSink && state.ctx) {
            try { await state.ctx.setSinkId(''); } catch (_) {}
        }
        if (state.path === 'netEQ' && state.audioEl && typeof state.audioEl.setSinkId === 'function') {
            try { await state.audioEl.setSinkId(''); } catch (_) {}
        }
    }

    async function ensureCategoryBridge() {
        if (!state.ctx || !state.gain) return;
        if (!speakers.msd) {
            speakers.msd = state.ctx.createMediaStreamDestination();
            try { state.gain.disconnect(state.ctx.destination); } catch (_) {}
            state.gain.connect(speakers.msd);
        }
        if (!speakers.sinkEl) {
            const a = document.createElement('audio');
            a.autoplay = true;
            a.playsInline = true;
            a.setAttribute('webkit-playsinline', '');
            a.muted = false;
            a.style.display = 'none';
            document.body.appendChild(a);
            speakers.sinkEl = a;
        }
        speakers.sinkEl.srcObject = speakers.msd.stream;
        const pr = speakers.sinkEl.play();
        if (pr && pr.catch) pr.catch(() => {});
    }

    function startDeviceWatch() {
        if (speakers.watching) return;
        speakers.watching = true;
        if (!speakers.canSink) return;
        const md = navigator.mediaDevices;
        const onChange = () => {
            if (speakers.firstDeviceChange) { speakers.firstDeviceChange = false; return; }
            if (speakers.debounceTimer) clearTimeout(speakers.debounceTimer);
            speakers.debounceTimer = setTimeout(reEnumerateAndPost, 500);
        };
        try {
            if (md && md.addEventListener && speakers.hasDeviceChange) {
                md.addEventListener('devicechange', onChange);
            }
        } catch (_) {}
        try {
            speakers.poll = setInterval(() => {
                if (document.visibilityState !== 'visible') return;
                reEnumerateAndPost();
            }, 4000);
        } catch (_) {}
    }

    async function reEnumerateAndPost() {
        try {
            let enumerated = speakers.canSink ? await listOutputs() : [];
            if (speakers.canSink) {
                const hasLabels = enumerated.some((d) => d.label);
                if ((!enumerated.length || !hasLabels) && speakers._lastEnum && speakers._lastEnum.length) {
                    enumerated = speakers._lastEnum;
                } else if (enumerated.length && hasLabels) {
                    speakers._lastEnum = enumerated;
                }
            }
            speakers._cache = { supported: speakers.supported, outputs: buildOutputs(enumerated) };
            postOutputs();
        } catch (_) {}
    }

    function postOutputs() {
        try {
            const cache = speakers._cache || { supported: speakers.supported, outputs: [] };
            const body = JSON.stringify({
                sessionId: speakers.sessionId || '',
                deviceToken: speakers.deviceToken || '',
                supported: cache.supported,
                outputs: cache.outputs,
                selected: speakers.appliedSink || '',
            });
            fetch('/audio-outputs', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body,
            }).catch(() => {});
        } catch (_) {}
    }

    async function setSpeaker(deviceId) {
        initSpeakers();
        const raw = deviceId || '';
        const id = (raw === 'default') ? '' : raw;
        if (speakers.appliedSink === id) return;

        if (id === SE_EARPIECE || id === SE_LOUDSPEAKER) {
            if (state.path === 'netEQ') { try { await revertSink(); } catch (_) {} }
            else { try { await ensureCategoryBridge(); } catch (_) {} }
            if (id === SE_EARPIECE) {
                applySession('play-and-record');
                if (!speakers.hasAudioSession && speakers.hasEarpiece)
                    { try { await acquireMicHold(); } catch (_) {} }
            } else {
                applySession('playback');
                releaseMicHold();
            }
            speakers.appliedSink = id;
            return;
        }

        applySession('playback');
        releaseMicHold();

        try {
            if (state.path === 'netEQ' && state.audioEl) {
                if (typeof state.audioEl.setSinkId === 'function') {
                    await state.audioEl.setSinkId(id);
                    speakers.appliedSink = id;
                }
                return;
            }
            if (!state.ctx || !state.gain) return;

            if (id === '') {
                await revertSink();
                speakers.appliedSink = '';
                return;
            }

            if (speakers.hasCtxSink) {
                await state.ctx.setSinkId(id);
                speakers.appliedSink = id;
                return;
            }

            if (speakers.hasElemSink) {
                if (!speakers.msd) {
                    speakers.msd = state.ctx.createMediaStreamDestination();
                    try { state.gain.disconnect(state.ctx.destination); } catch (_) {}
                    state.gain.connect(speakers.msd);
                }
                if (!speakers.sinkEl) {
                    const a = document.createElement('audio');
                    a.autoplay = true;
                    a.playsInline = true;
                    a.setAttribute('webkit-playsinline', '');
                    a.muted = false;
                    a.style.display = 'none';
                    document.body.appendChild(a);
                    speakers.sinkEl = a;
                }
                speakers.sinkEl.srcObject = speakers.msd.stream;
                await speakers.sinkEl.setSinkId(id);
                const p = speakers.sinkEl.play();
                if (p && p.catch) p.catch(() => {});
                speakers.appliedSink = id;
                return;
            }
        } catch (_) {}
    }

    function setSpeakerIdentity(sessionId, deviceToken) {
        speakers.sessionId = sessionId || '';
        speakers.deviceToken = deviceToken || '';
    }

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
        initSpeakers();
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
            a.setAttribute('webkit-playsinline', '');
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
        initSpeakers,
        unlockAndEnumerate,
        needsMicPermission,
        primeMicPermission,
        micReady,
        postOutputs,
        setSpeaker,
        setSpeakerIdentity,
        _enumPromise: null,
    };
})();

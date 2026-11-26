(function () {
    'use strict';

    const SR = 48000;
    const CH = 2;
    const MIN_TARGET_MS = 10;
    const MAX_TARGET_MS = 400;
    const P_MS = 0x100000000 / 90;
    const HDR_BYTES = 13;
    // Planar audio ring ABI. Hand-duplicated in audio-worker.js (the writer) and
    // audio-worklet.js (the reader); change all three together.
    //
    //   [0 .. 64)                        control block, 16 Int32
    //                                      [0] W  absolute write position, frames
    //                                      [1] R  absolute read position, frames
    //                                      [2] OVERRUNS
    //   [64 .. 64 + cap*4)               left channel,  Float32
    //   [64 + cap*4 .. 64 + cap*8)       right channel, Float32
    //
    // Positions are absolute uint32 frame counters and the index is `pos & (cap-1)`, so the
    // capacity must stay a power of two.
    const CTRL_BYTES = 64;
    const CAP_FRAMES = 65536;

    let suppressed = 0;
    function note() { suppressed++; }

    function detectCapabilities() {
        const AC = window.AudioContext || window.webkitAudioContext;
        const worklet = !!(AC && AC.prototype && 'audioWorklet' in AC.prototype);
        const webcodecsOpus =
            typeof window.AudioDecoder === 'function' &&
            typeof window.EncodedAudioChunk === 'function' &&
            typeof window.AudioData === 'function';
        // `redundancy` is a property of this build: audio-worker.js can split a FLAG_REDUNDANT
        // body. The host sets the flag only when told so, because a client that cannot split
        // decodes the two payloads as one packet.
        return { webcodecsOpus, worklet, redundancy: true };
    }

    // The host reads the capability answer once: on the fast path it sends Opus on a DataChannel
    // with no audio track in the SDP, so a decoder that then refuses `opus` is permanent silence.
    // Hence a real decode probe; AudioDecoder is new in Safari 26 and Firefox 130.
    // Probe: a CELT-only mono fullband 20 ms frame, TOC 0xf8 (config 31, stereo clear, code 0).
    const OPUS_PROBE = new Uint8Array([0xf8, 0xff, 0xfe]);
    const DECODE_PROBE_DEADLINE_MS = 400;

    // isConfigSupported says nothing about AudioData.copyTo, which deinterleaves into the ring: an
    // engine that accepts `opus` and refuses the f32-planar conversion drops every packet. Only a
    // failed copyTo or configure demotes. A decoder error or a timeout is inconclusive (the probe
    // packet may be at fault); the per-packet counter in audio-worker.js catches that.
    function probeOpusDecode() {
        return new Promise(function (resolve) {
            let dec = null;
            let settled = false;
            let timer = 0;
            const done = function (v) {
                if (settled) return;
                settled = true;
                if (timer) clearTimeout(timer);
                try { if (dec && dec.state !== 'closed') dec.close(); } catch (_) { note(); }
                resolve(v);
            };
            timer = setTimeout(function () { done('inconclusive'); }, DECODE_PROBE_DEADLINE_MS);
            try {
                dec = new AudioDecoder({
                    output: function (ad) {
                        let okCopy = true;
                        try {
                            const n = ad.numberOfFrames || 1;
                            ad.copyTo(new Float32Array(n), { planeIndex: 0, format: 'f32-planar' });
                        } catch (_) { okCopy = false; }
                        try { ad.close(); } catch (_) { note(); }
                        done(okCopy ? 'ok' : 'copy-failed');
                    },
                    error: function () { done('decode-error'); },
                });
                dec.configure({ codec: 'opus', sampleRate: SR, numberOfChannels: CH });
                dec.decode(new EncodedAudioChunk({ type: 'key', timestamp: 0, data: OPUS_PROBE }));
            } catch (_) {
                done('unsupported');
            }
        });
    }

    let verified = null;
    async function verifyCapabilities() {
        if (verified) return verified;
        const caps = detectCapabilities();
        if (caps.webcodecsOpus && typeof AudioDecoder.isConfigSupported === 'function') {
            try {
                const s = await AudioDecoder.isConfigSupported({
                    codec: 'opus', sampleRate: SR, numberOfChannels: CH,
                });
                caps.webcodecsOpus = !!(s && s.supported);
            } catch (_) {
                note();
                caps.webcodecsOpus = false;
            }
        }
        if (caps.webcodecsOpus) {
            let outcome = 'inconclusive';
            try { outcome = await probeOpusDecode(); } catch (_) { note(); }
            if (outcome === 'copy-failed' || outcome === 'unsupported') {
                console.warn('[audio] Opus decode probe: ' + outcome + '; taking the standard track');
                caps.webcodecsOpus = false;
            } else if (outcome !== 'ok') {
                note();
            }
        }
        // the answer came from a decoder probe, not a constructor sniff; the host logs it per join
        caps.verified = true;
        // WHEP has no renegotiation verb, so a spare audio track must be in the first answer. Ask
        // only on the fast path, where the host otherwise sends no audio track and the decoder can
        // still fail later; off it the host already feeds a track.
        caps.wantsFallbackTrack = !!(caps.webcodecsOpus && caps.worklet);
        verified = caps;
        return caps;
    }

    function sharedMemoryOk() {
        return self.crossOriginIsolated === true &&
            typeof SharedArrayBuffer === 'function' &&
            typeof Atomics === 'object';
    }

    const state = {
        ctx: null,
        node: null,
        gain: null,
        worker: null,
        sab: null,
        dc: null,
        audioEl: null,
        fallbackStream: null,
        path: null, // 'webcodecs' | 'netEQ' | null
        muted: false,
        prepared: false,
        stats: null,
        videoDelayMs: null,     // from the video worker (EMA of display lag)
        hostOffsetMs: null,     // host<->local clock offset, from SEClock via PING/PONG
        preEnqueueEmaMs: null,  // audio capture-to-enqueue latency, smoothed
        lastTargetMs: null,     // last depth commanded to the worklet
        lastTargetAt: 0,
        residualOffsetMs: 0,    // unachievable sync error after clamping (diagnostics)
        lastOffsetLogAt: 0,
        outputLatencyMs: 0,
        outputLatencyEmaMs: null,
        outputLatencySource: 'unavailable',
        safetyMs: 0,            // adaptive margin on top of the sync target
        lastUnderruns: null,
        cleanBlocks: 0,
        concealed: 0,
        redundantUsed: 0,
        redundantDiscarded: 0,
        decoderFailed: false,
        prepareFailed: false,
    };

    const SE_LOUDSPEAKER = 'se:loudspeaker';
    const SE_EARPIECE = 'se:earpiece';
    // not a device and not offered by this page; a host row saved by an older page may carry it
    const SE_CHOOSE = 'se:choose';

    const speakers = {
        inited: false,
        hasEnumerate: false,
        hasElemSink: false,
        hasCtxSink: false,
        hasSelectOut: false,
        hasSinkApi: false,
        insecure: false,
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
        // whether the posted list carries real device names; `null` until something has enumerated
        outputsLabelled: null,
        sinkWatched: false,
        msd: null,
        sinkEl: null,
        micHold: null,
        micGranted: false,
        // `{ id, fromPicker }`: a sink to route once the graph exists. Only a chooser pick may
        // claim `SINK_TIER.selectAudioOutput`, so the provenance is carried, not inferred.
        pendingSink: null,
        sessionId: '',
        deviceToken: '',
        labelsUnavailable: false,
        refusedSink: null,
        onDeviceChange: null,
    };

    function detectIOS() {
        try {
            const ua = navigator.userAgent || '';
            const iPad = navigator.platform === 'MacIntel' && (navigator.maxTouchPoints || 0) > 1;
            return /iPad|iPhone|iPod/.test(ua) || iPad;
        } catch (_) { return false; }
    }

    // No capability says "has an earpiece" (`navigator.audioSession` exists on iPad too), so it is
    // inferred: iOS minus the iPads. A bare /iPhone/ test misses an iPhone with "Request Desktop
    // Website" on, which sends a Macintosh UA. Speaker/Earpiece is the only output control iOS has
    // below 18.4, where HTMLMediaElement.setSinkId arrives.
    function detectHasEarpiece() {
        try {
            if (!detectIOS()) return false;
            const ua = navigator.userAgent || '';
            if (/iPhone|iPod/.test(ua)) return true;
            if (/iPad/.test(ua)) return false;
            // desktop-mode UA: an iPhone's short edge is at most 440 CSS px, an iPad's at least 744
            const s = window.screen || {};
            const shortEdge = Math.min(s.width || 0, s.height || 0);
            return shortEdge > 0 && shortEdge <= 500;
        } catch (_) { note(); return false; }
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
            // `setSinkId`, `selectAudioOutput` and `navigator.mediaDevices` are secure-context
            // only: on plain HTTP their absence is the connection's doing, not the browser's
            speakers.insecure = window.isSecureContext === false;
            speakers.hasSinkApi = speakers.hasElemSink || speakers.hasCtxSink || speakers.hasSelectOut;
            // `false` makes the host say this browser cannot switch speakers, so it is sent only
            // with no sink API, no earpiece category, and not merely because the page is on HTTP
            speakers.supported = speakers.hasSinkApi || speakers.canCategory || speakers.insecure;
        } catch (_) { note(); }
        if (speakers.hasAudioSession) {
            try { navigator.audioSession.type = 'playback'; } catch (_) { note(); }
        }
    }

    // Output discovery runs before the offer: each API in a fixed order under its own timeout,
    // yielding `ok`, `unsupported`, `denied`, `empty`, `error` or `timeout`; the microphone card
    // only if none gave a routable output. Ungranted, Chrome and Edge 154 list one unroutable
    // `audiooutput` (empty id and label) and throw `TypeError` on the `speaker-selection` query.
    const DISCOVERY_MS = { picker: 30000, permission: 2000, enumerate: 3000, microphone: 30000 };

    function classify(e) {
        const name = e && e.name ? e.name : '';
        if (name === 'NotAllowedError' || name === 'SecurityError') return 'denied';
        if (name === 'NotFoundError') return 'empty';
        return 'error';
    }

    // fn may throw synchronously as well as reject
    function attempt(fn) {
        try { return Promise.resolve(fn()); } catch (e) { return Promise.reject(e); }
    }

    // a timeout is a definite failure of that API
    function bounded(promise, ms, label) {
        let timer = 0;
        return Promise.race([
            promise.then(function (v) { return { label: label, result: 'ok', value: v }; },
                         function (e) { return { label: label, result: classify(e), error: e }; }),
            new Promise(function (resolve) {
                timer = setTimeout(function () { resolve({ label: label, result: 'timeout' }); }, ms);
            }),
        ]).then(function (r) {
            if (timer) clearTimeout(timer);
            return r;
        });
    }

    // called once the result is final, so an expected answer (an unknown permission name's
    // `TypeError`) is not counted as a suppressed failure
    function counted(r) {
        if (r.result === 'error' || r.result === 'timeout') note();
        return r;
    }

    // The browser's own chooser (Firefox 116+, Chromium behind a flag). It needs the Connect
    // click's transient activation: call it synchronously before the first await, and only once (a
    // second call has no activation left). A dismissed chooser is `denied`.
    let pickerRun = null;
    function startPicker() {
        initSpeakers();
        const md = navigator.mediaDevices;
        if (!md || typeof md.selectAudioOutput !== 'function') {
            pickerRun = Promise.resolve({ label: 'selectAudioOutput', result: 'unsupported' });
        } else {
            pickerRun = bounded(attempt(function () { return md.selectAudioOutput(); }), DISCOVERY_MS.picker, 'selectAudioOutput')
                .then(function (r) {
                    if (r.result === 'ok' && !(r.value && r.value.deviceId)) r.result = 'empty';
                    if (r.result === 'ok') {
                        // applied once the graph exists
                        speakers.pendingSink = { id: r.value.deviceId, fromPicker: true };
                    }
                    return counted(r);
                });
        }
        return pickerRun;
    }

    // an engine that does not know the permission name throws `TypeError`: `unsupported`
    function queryPermission(name) {
        const label = 'permissions.' + name;
        if (!navigator.permissions || typeof navigator.permissions.query !== 'function') {
            return Promise.resolve({ label: label, result: 'unsupported' });
        }
        return bounded(attempt(function () { return navigator.permissions.query({ name: name }); }), DISCOVERY_MS.permission, label)
            .then(function (r) {
                if (r.result === 'error' && r.error && r.error.name === 'TypeError') r.result = 'unsupported';
                if (r.result === 'ok') r.state = (r.value && typeof r.value.state === 'string') ? r.value.state : 'unknown';
                return counted(r);
            });
    }
    function querySpeakerPermission() { return queryPermission('speaker-selection'); }
    function queryMicrophonePermission() { return queryPermission('microphone'); }

    // An empty `deviceId` is Chromium's pre-grant placeholder and cannot be routed to; `default`
    // and `communications` are real ids and are kept.
    function enumerateOutputs(label) {
        const md = navigator.mediaDevices;
        const l = label || 'enumerateDevices';
        if (!md || typeof md.enumerateDevices !== 'function') {
            return Promise.resolve({ label: l, result: 'unsupported' });
        }
        return bounded(attempt(function () { return md.enumerateDevices(); }), DISCOVERY_MS.enumerate, l).then(function (r) {
            if (r.result !== 'ok') return counted(r);
            const outs = [];
            const list = r.value || [];
            for (let i = 0; i < list.length; i++) {
                const d = list[i];
                if (d && d.kind === 'audiooutput' && d.deviceId) outs.push({ id: d.deviceId, label: d.label || '' });
            }
            r.value = outs;
            if (!outs.length) r.result = 'empty';
            return r;
        });
    }

    // Last resort, only from the page's own card: one microphone request, one enumeration while it
    // is held, then every track stopped.
    function primeWithMicrophone() {
        const md = navigator.mediaDevices;
        if (!md || typeof md.getUserMedia !== 'function') {
            return Promise.resolve({ gum: { label: 'getUserMedia', result: 'unsupported' }, retry: null });
        }
        const asked = attempt(function () { return md.getUserMedia({ audio: true }); });
        return bounded(asked, DISCOVERY_MS.microphone, 'getUserMedia').then(function (r) {
            if (r.result !== 'ok') {
                // a prompt answered after its deadline must not leave the microphone open
                if (r.result === 'timeout') {
                    asked.then(function (s) { try { s.getTracks().forEach(function (t) { t.stop(); }); } catch (_) { note(); } }, function () {});
                }
                return { gum: counted(r), retry: null };
            }
            speakers.micGranted = true;
            const stream = r.value;
            return enumerateOutputs('enumerateDevices.afterMicrophone').then(function (again) {
                try { stream.getTracks().forEach(function (t) { t.stop(); }); } catch (_) { note(); }
                r.value = null;
                return { gum: r, retry: again };
            });
        });
    }

    // a labelled list already seen on this page is not replaced by a later unlabelled or empty one
    function listFrom(picker, enumerated) {
        const out = [];
        const seen = {};
        const add = function (d) {
            if (!d || !d.id || seen[d.id]) return;
            seen[d.id] = true;
            out.push({ id: d.id, label: d.label || '' });
        };
        if (picker && picker.result === 'ok' && picker.value) add({ id: picker.value.deviceId, label: picker.value.label });
        if (enumerated && enumerated.result === 'ok') enumerated.value.forEach(add);
        const labelled = out.some(function (d) { return d.label; });
        if (labelled) speakers._lastEnum = out;
        else if (speakers._lastEnum && speakers._lastEnum.length) return speakers._lastEnum;
        return out;
    }

    function discoveryMap(r) {
        const m = {};
        const order = ['picker', 'speakerPerm', 'micPerm', 'enumerate', 'ctxSink', 'elSink', 'session', 'deviceChange', 'getUserMedia', 'retry'];
        for (let i = 0; i < order.length; i++) {
            const x = r[order[i]];
            if (!x) continue;
            m[x.label] = x.result;
            if (x.state) m[x.label + '.state'] = x.state;
        }
        return m;
    }

    let lastPlan = null;
    let lastDiscovery = null;
    // Run by the Join handler before the offer. `showCard(onContinue, onSkip)` raises the
    // microphone card, which always has Skip, so this never waits on what the user cannot end.
    async function prepareOutputsForJoin(showCard, pick) {
        initSpeakers();
        const r = {};
        r.picker = await (pick || pickerRun || startPicker());
        pickerRun = null;
        r.speakerPerm = await querySpeakerPermission();
        r.micPerm = await queryMicrophonePermission();
        r.enumerate = await enumerateOutputs();
        r.ctxSink = { label: 'AudioContext.setSinkId', result: speakers.hasCtxSink ? 'ok' : 'unsupported' };
        r.elSink = { label: 'HTMLMediaElement.setSinkId', result: speakers.hasElemSink ? 'ok' : 'unsupported' };
        r.session = { label: 'audioSession', result: speakers.hasAudioSession ? 'ok' : 'unsupported' };
        r.deviceChange = { label: 'devicechange', result: speakers.hasDeviceChange ? 'ok' : 'unsupported' };

        let devices = listFrom(r.picker, r.enumerate);
        const plan = { action: 'report', reason: devices.length ? 'discovered' : (speakers.canCategory ? 'audio-session' : '') };
        if (!devices.length && !speakers.canCategory) {
            // nothing routable: raise the card only where a grant could change that
            const md = navigator.mediaDevices;
            if (!md || typeof md.getUserMedia !== 'function') {
                r.getUserMedia = { label: 'getUserMedia', result: 'unsupported' };
                plan.reason = 'no-getUserMedia';
            } else if (r.micPerm.state === 'denied') {
                r.getUserMedia = { label: 'getUserMedia', result: 'denied' };
                plan.reason = 'mic-denied';
            } else if (!speakers.hasSinkApi) {
                // no API could route a list even if a grant revealed one
                r.getUserMedia = { label: 'getUserMedia', result: 'unsupported' };
                plan.reason = 'no-sink-api';
            } else if (typeof showCard === 'function') {
                plan.action = 'card';
                const go = await new Promise(function (resolve) {
                    try { showCard(function () { resolve(true); }, function () { resolve(false); }); } catch (_) { note(); resolve(false); }
                });
                if (!go) {
                    r.getUserMedia = { label: 'getUserMedia', result: 'denied' };
                    plan.answer = 'skipped';
                } else {
                    const primed = await primeWithMicrophone();
                    r.getUserMedia = primed.gum;
                    if (primed.retry) r.retry = primed.retry;
                    plan.answer = primed.gum.result === 'ok' ? 'granted' : 'refused';
                    devices = primed.retry ? listFrom(null, primed.retry) : devices;
                }
                plan.reason = devices.length ? 'after-microphone' : 'after-microphone-nothing';
            }
            if (!devices.length) plan.action = plan.action === 'card' ? 'card-default-only' : 'default-only';
        }
        noteOutputLabels(devices);
        lastDiscovery = discoveryMap(r);
        speakers._cache = { supported: speakers.supported, outputs: buildOutputs(devices) };
        plan.outputs = speakers._cache.outputs.length;
        plan.labelled = speakers.outputsLabelled;
        plan.discovery = lastDiscovery;
        enumStarted = true;
        startDeviceWatch();
        lastPlan = plan;
        return plan;
    }

    // for a caller that only wants the cached report (a second join on the same page, a harness)
    let enumStarted = false;
    async function enumerateBeforeJoin() {
        if (enumStarted) return speakers._cache || { supported: speakers.supported, outputs: buildOutputs([]) };
        enumStarted = true;
        initSpeakers();
        const r = await enumerateOutputs();
        const devices = listFrom(null, r);
        noteOutputLabels(devices);
        speakers._cache = { supported: speakers.supported, outputs: buildOutputs(devices) };
        startDeviceWatch();
        return speakers._cache;
    }

    // a saved host row may still carry SE_CHOOSE: ignored here and in `setSpeaker`
    function noteOutputChoiceRequest(sink) {
        return sink === SE_CHOOSE ? 'ignored' : 'cleared';
    }

    function getOutputPlan() { return lastPlan; }

    async function listOutputs() {
        const r = await enumerateOutputs();
        return r.result === 'ok' ? r.value : [];
    }

    // Unlabelled ids are routable but unreadable: `buildOutputs` names them by position and the
    // host is told `labelled: false`, so it never presents them as the OS's own names.
    function noteOutputLabels(devices) {
        const labelled = devices.length ? devices.some(function (d) { return d.label; }) : speakers.canCategory;
        speakers.outputsLabelled = !!labelled;
        speakers.labelsUnavailable = !labelled;
    }

    // What the host is told: the earpiece categories where they apply, every routable device, and,
    // with nothing else, exactly one option, the default output.
    function buildOutputs(devices) {
        const out = [];
        if (speakers.canCategory) {
            out.push({ id: SE_LOUDSPEAKER, label: 'Speaker' });
            out.push({ id: SE_EARPIECE, label: 'Earpiece' });
        }
        let n = 0;
        for (let i = 0; i < devices.length; i++) {
            const d = devices[i];
            if (d.label) { out.push({ id: d.id, label: d.label }); continue; }
            n++;
            out.push({ id: d.id, label: d.id === 'default' ? 'Default output' : 'Output ' + n });
        }
        if (!out.length) out.push({ id: '', label: 'Default output' });
        return out;
    }

    function applySession(type) {
        if (!speakers.hasAudioSession) return false;
        try { navigator.audioSession.type = type; return true; } catch (_) { return false; }
    }

    async function acquireMicHold() {
        if (speakers.micHold) return true;
        // Only for the earpiece on an engine with no audioSession category (iOS before 16.4), where
        // holding the microphone is the routing mechanism. The one permission the page may raise
        // mid-session: an iPhone has no fullscreen or pointer lock for the prompt to drop.
        try {
            speakers.micHold = await navigator.mediaDevices.getUserMedia({ audio: true });
            speakers.micGranted = true;
            return true;
        } catch (_) { speakers.micHold = null; return false; }
    }

    function releaseMicHold() {
        if (!speakers.micHold) return;
        try { speakers.micHold.getTracks().forEach((t) => t.stop()); } catch (_) { note(); }
        speakers.micHold = null;
    }

    async function revertSink() {
        if (speakers.msd) {
            try { state.gain.disconnect(speakers.msd); } catch (_) { note(); }
            try { if (state.ctx) state.gain.connect(state.ctx.destination); } catch (_) { note(); }
            if (speakers.sinkEl) { try { speakers.sinkEl.pause(); } catch (_) { note(); } speakers.sinkEl.srcObject = null; }
            speakers.msd = null;
        } else if (speakers.hasCtxSink && state.ctx) {
            try { await state.ctx.setSinkId(''); } catch (_) { note(); }
        }
        if (state.path === 'netEQ' && state.audioEl && typeof state.audioEl.setSinkId === 'function') {
            try { await state.audioEl.setSinkId(''); } catch (_) { note(); }
        }
    }

    async function ensureCategoryBridge() {
        if (!state.ctx || !state.gain) return;
        if (!speakers.msd) {
            speakers.msd = state.ctx.createMediaStreamDestination();
            try { state.gain.disconnect(state.ctx.destination); } catch (_) { note(); }
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
            // a refused id may be one that was just plugged in
            speakers.refusedSink = null;
            if (speakers.debounceTimer) clearTimeout(speakers.debounceTimer);
            speakers.debounceTimer = setTimeout(reEnumerateAndPost, 500);
        };
        try {
            if (md && md.addEventListener && speakers.hasDeviceChange) {
                md.addEventListener('devicechange', onChange);
                speakers.onDeviceChange = onChange;
            }
        } catch (_) { note(); }
        // The poll is the backstop for a browser with no `devicechange` event. With the event, 30 s
        // is only a liveness check: enumerating every 4 s on the thread that forwards each 5 ms
        // audio packet buys nothing.
        const period = speakers.hasDeviceChange ? 30000 : 4000;
        try {
            speakers.poll = setInterval(() => {
                if (document.visibilityState !== 'visible') return;
                reEnumerateAndPost();
            }, period);
        } catch (_) { note(); }
    }

    // the watch belongs to a session; the next Connect's discovery starts it again
    function stopDeviceWatch() {
        speakers.watching = false;
        speakers.firstDeviceChange = true;
        if (speakers.poll) { clearInterval(speakers.poll); speakers.poll = null; }
        if (speakers.debounceTimer) { clearTimeout(speakers.debounceTimer); speakers.debounceTimer = 0; }
        const md = navigator.mediaDevices;
        if (speakers.onDeviceChange) {
            try { if (md && md.removeEventListener) md.removeEventListener('devicechange', speakers.onDeviceChange); } catch (_) { note(); }
            speakers.onDeviceChange = null;
        }
    }

    // Where the audio is actually playing, read back rather than assumed: the user can move the
    // sink in the browser's own UI (Firefox's per-tab output control, `selectAudioOutput()`).
    function readCurrentSink() {
        try {
            if (state.path === 'netEQ' && state.audioEl && typeof state.audioEl.sinkId === 'string') {
                return state.audioEl.sinkId;
            }
            if (speakers.sinkEl && typeof speakers.sinkEl.sinkId === 'string') return speakers.sinkEl.sinkId;
            // `AudioContext.sinkId` is a string for a device id and an object for the
            // silent sink, which is not a device and must not be reported as one.
            if (state.ctx && typeof state.ctx.sinkId === 'string') return state.ctx.sinkId;
        } catch (_) { note(); }
        return null;
    }

    function syncCurrentSink() {
        const live = readCurrentSink();
        if (live === null) return false;
        const norm = (live === 'default') ? '' : live;
        if (norm === speakers.appliedSink) return false;
        speakers.appliedSink = norm;
        postOutputs();
        return true;
    }

    // where `sinkchange` does not exist the device poll picks the same change up within its period
    function watchSinkChanges() {
        if (speakers.sinkWatched) return;
        speakers.sinkWatched = true;
        try {
            if (state.ctx && state.ctx.addEventListener) {
                state.ctx.addEventListener('sinkchange', () => { syncCurrentSink(); });
            }
        } catch (_) { note(); }
    }

    async function reEnumerateAndPost() {
        syncCurrentSink();
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
            noteOutputLabels(enumerated);
            speakers._cache = { supported: speakers.supported, outputs: buildOutputs(enumerated) };
            postOutputs();
        } catch (_) { note(); }
    }

    let lastPostedOutputs = null;
    function postOutputs(force) {
        // enumeration runs before the join, when the host has no session to attach a list to;
        // `setSpeakerIdentity` clears the change detection so the first real post goes out
        if (!speakers.sessionId) return;
        // `speakers.supported` is `false` until the probe runs; a post must never carry that
        initSpeakers();
        try {
            const cache = speakers._cache || { supported: speakers.supported, outputs: buildOutputs([]) };
            const body = JSON.stringify({
                sessionId: speakers.sessionId || '',
                deviceToken: speakers.deviceToken || '',
                supported: cache.supported,
                outputs: cache.outputs,
                selected: speakers.appliedSink || '',
                // What this device is playing on. The host ignores the posted `selected`, so this
                // is the only way a change made in the browser's own output control reaches it.
                // `undefined` (dropped by `JSON.stringify`) until a sink has been applied, so the
                // host can tell "never said" from `''`, the default output.
                current: speakers.appliedSink === null ? undefined : speakers.appliedSink,
                // `false`: the names are this page's own ("Output 1") because the browser withheld
                // the real ones. Omitted until something has enumerated ("not said").
                labelled: speakers.outputsLabelled === null ? undefined : speakers.outputsLabelled,
                // what each discovery API answered at the join, keyed by API
                discovery: lastDiscovery || undefined,
            });
            // the list rarely changes and the host does the same work either way: skip a repeat
            if (!force && body === lastPostedOutputs) return;
            lastPostedOutputs = body;
            // the host attaches the list by join id and will not guess a session from the address
            if (!speakers.joinId) return;
            fetch('/audio-outputs?join=' + encodeURIComponent(speakers.joinId), {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body,
            }).catch(() => {});
        } catch (_) { note(); }
    }

    // Which mechanism actually moved the audio, reported to the host so a device where the
    // sink selection silently does nothing is distinguishable from one where it worked.
    const SINK_TIER = { none: 0, ctxSetSinkId: 1, elementSetSinkId: 2, audioSession: 3, selectAudioOutput: 4 };
    let sinkTier = SINK_TIER.none;

    // Resolves `true` when the audio moved, `false` when a mechanism was tried and refused, and
    // `null` when there is no graph yet: the sink is remembered for `applyPendingSink`.
    async function setSpeaker(deviceId) {
        initSpeakers();
        const raw = deviceId || '';
        const id = (raw === 'default') ? '' : raw;
        if (speakers.appliedSink === id) return true;
        // a request id, not an output: routing stays where it is
        if (id === SE_CHOOSE) return true;
        // An id this engine already refused is still saved on the host and rides every /reconfig
        // poll: refuse it once, not once a second. A device change clears the latch.
        if (speakers.refusedSink === id) return false;

        if (id === SE_EARPIECE || id === SE_LOUDSPEAKER) {
            if (state.path === 'netEQ') { try { await revertSink(); } catch (_) { note(); } }
            else { try { await ensureCategoryBridge(); } catch (_) { note(); } }
            if (id === SE_EARPIECE) {
                if (applySession('play-and-record')) sinkTier = SINK_TIER.audioSession;
                // with no audioSession category the microphone hold is what routes to the earpiece;
                // it is released as soon as the loudspeaker is chosen again
                if (!speakers.hasAudioSession && speakers.hasEarpiece) {
                    try { if (await acquireMicHold()) sinkTier = SINK_TIER.audioSession; } catch (_) { note(); }
                }
            } else {
                if (applySession('playback')) sinkTier = SINK_TIER.audioSession;
                releaseMicHold();
            }
            speakers.appliedSink = id;
            postOutputs();
            return true;
        }

        applySession('playback');
        releaseMicHold();

        try {
            if (state.path === 'netEQ' && state.audioEl) {
                if (typeof state.audioEl.setSinkId !== 'function') return false;
                await state.audioEl.setSinkId(id);
                speakers.appliedSink = id;
                postOutputs();
                return true;
            }
            // no graph yet, so nothing has refused: remember it rather than report a failure
            if (!state.ctx || !state.gain) {
                speakers.pendingSink = { id: raw, fromPicker: false };
                return null;
            }

            if (id === '') {
                await revertSink();
                speakers.appliedSink = '';
                postOutputs();
                return true;
            }

            // own try: a NotFoundError (device gone) or NotAllowedError falls through to the
            // element rung below instead of the shared catch
            if (speakers.hasCtxSink) {
                let ctxSinkOk = false;
                try {
                    await state.ctx.setSinkId(id);
                    ctxSinkOk = true;
                } catch (_) { note(); }
                if (ctxSinkOk) {
                    speakers.appliedSink = id;
                    sinkTier = SINK_TIER.ctxSetSinkId;
                    // a different device has a different buffer depth: re-derive the A/V target
                    state.outputLatencyEmaMs = null;
                    maybeUpdateTarget();
                    postOutputs();
                    return true;
                }
            }

            if (speakers.hasElemSink) {
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
                // sink first, graph second: rerouted early, a refused id plays on the default
                // device, and assigning `srcObject` restarts the element
                await speakers.sinkEl.setSinkId(id);
                if (!speakers.msd) {
                    speakers.msd = state.ctx.createMediaStreamDestination();
                    try { state.gain.disconnect(state.ctx.destination); } catch (_) { note(); }
                    state.gain.connect(speakers.msd);
                }
                if (speakers.sinkEl.srcObject !== speakers.msd.stream) speakers.sinkEl.srcObject = speakers.msd.stream;
                const p = speakers.sinkEl.play();
                if (p && p.catch) p.catch(note);
                speakers.appliedSink = id;
                sinkTier = SINK_TIER.elementSetSinkId;
                state.outputLatencyEmaMs = null;
                maybeUpdateTarget();
                postOutputs();
                return true;
            }
        } catch (_) { note(); }
        speakers.refusedSink = id;
        return false;
    }

    // Routes a sink chosen before the graph existed. The Join click runs `selectAudioOutput()`, the
    // only permission-free picker and the only one that gives a label without a microphone grant,
    // before there is an AudioContext. The tier is claimed only if the sink actually moved.
    function applyPendingSink() {
        const pending = speakers.pendingSink;
        if (!pending) return Promise.resolve(false);
        speakers.pendingSink = null;
        const raw = pending.id;
        const want = (raw === 'default') ? '' : raw;
        return Promise.resolve(setSpeaker(raw)).then(function () {
            if (speakers.appliedSink !== want) return false;
            // only a chooser pick is tier 4; `setSpeaker` already recorded a host-commanded one
            if (pending.fromPicker) sinkTier = SINK_TIER.selectAudioOutput;
            return true;
        }, function () { note(); return false; });
    }

    function setSpeakerIdentity(sessionId, deviceToken, joinId) {
        speakers.sessionId = sessionId || '';
        speakers.deviceToken = deviceToken || '';
        // each tab posts under its own join, so a late list cannot land on another tab's row
        speakers.joinId = joinId || '';
        // a new or rebuilt session needs the list once even if unchanged: reset change detection
        lastPostedOutputs = null;
    }

    // Graph-to-speaker delay. `outputLatency` (Chrome 102, Firefox 70, Safari 18.4) is the direct
    // answer; reading its absence as zero makes the jitter depth too large by that much, 3-10 ms on
    // a wired output and 100-200 ms on Bluetooth. getOutputTimestamp() (Safari 14.1, Firefox 70,
    // Chrome 57) gives it indirectly: which sample the device is playing, and when.
    const OUTPUT_LATENCY_FLOOR_MS = 0;
    const OUTPUT_LATENCY_CEILING_MS = 500;
    function estimateOutputLatencyMs(c) {
        if (typeof c.getOutputTimestamp !== 'function') return null;
        let ts;
        try { ts = c.getOutputTimestamp(); } catch (_) { note(); return null; }
        if (!ts || typeof ts.contextTime !== 'number' || typeof ts.performanceTime !== 'number') return null;
        if (!(ts.contextTime > 0)) return null;
        const elapsedSec = (performance.now() - ts.performanceTime) / 1000;
        const playingContextTime = ts.contextTime + elapsedSec;
        const latency = (c.currentTime - playingContextTime) * 1000;
        if (!isFinite(latency)) return null;
        return Math.max(OUTPUT_LATENCY_FLOOR_MS, Math.min(OUTPUT_LATENCY_CEILING_MS, latency));
    }

    function outputLatencyMs() {
        const c = state.ctx;
        if (!c) return 0;
        const base = typeof c.baseLatency === 'number' ? c.baseLatency : 0;
        if (typeof c.outputLatency === 'number' && c.outputLatency > 0) {
            state.outputLatencySource = 'measured';
            state.outputLatencyMs = (base + c.outputLatency) * 1000;
            return state.outputLatencyMs;
        }
        const est = estimateOutputLatencyMs(c);
        if (est !== null) {
            // low-pass: getOutputTimestamp jitters by a quantum or two
            state.outputLatencyEmaMs = state.outputLatencyEmaMs === null
                ? est : state.outputLatencyEmaMs * 0.85 + est * 0.15;
            state.outputLatencySource = 'estimated';
            state.outputLatencyMs = base * 1000 + state.outputLatencyEmaMs;
            return state.outputLatencyMs;
        }
        state.outputLatencySource = base > 0 ? 'baseLatency' : 'unavailable';
        state.outputLatencyMs = base * 1000;
        return state.outputLatencyMs;
    }

    // Margin on top of the A/V depth that keeps the ring from starving on a jittery link: it grows
    // when the worklet runs dry and decays on a clean one, so a good network pays none.
    const SAFETY_STEP_MS = 4;
    const SAFETY_MAX_MS = 60;
    const SAFETY_DECAY_MS = 1;
    function adaptSafetyMargin(st) {
        const under = st.underruns || 0;
        if (state.lastUnderruns === null) { state.lastUnderruns = under; return; }
        const fresh = under - state.lastUnderruns;
        state.lastUnderruns = under;
        if (fresh > 0) {
            state.safetyMs = Math.min(SAFETY_MAX_MS, state.safetyMs + SAFETY_STEP_MS * Math.min(3, fresh));
            state.cleanBlocks = 0;
            maybeUpdateTarget();
            return;
        }
        // maybePostStats fires roughly every 256 ms, so eight clean reports is ~2 s
        state.cleanBlocks++;
        if (state.cleanBlocks >= 8 && state.safetyMs > 0) {
            state.cleanBlocks = 0;
            state.safetyMs = Math.max(0, state.safetyMs - SAFETY_DECAY_MS);
            maybeUpdateTarget();
        }
    }

    function maybeUpdateTarget() {
        if (state.path !== 'webcodecs' || !state.node) return;
        if (state.videoDelayMs === null || state.preEnqueueEmaMs === null) return;

        let raw = state.videoDelayMs - state.preEnqueueEmaMs - outputLatencyMs();
        raw = ((raw % P_MS) + P_MS) % P_MS;
        if (raw > P_MS / 2) raw -= P_MS;
        raw += state.safetyMs;

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

    // audio and video workers must share one clock offset: the sync target is their difference
    function setHostClockOffset(offsetMs) {
        if (typeof offsetMs !== 'number' || !isFinite(offsetMs)) return;
        state.hostOffsetMs = offsetMs;
        if (state.worker) state.worker.postMessage({ type: 'clockoffset', offsetMs: offsetMs });
    }

    function setVideoDelay(ms) {
        if (typeof ms !== 'number' || !isFinite(ms)) return;
        state.videoDelayMs = ms;
        maybeUpdateTarget();
    }

    // A page open on the host machine plays into the mix the host is capturing: an echo that
    // repeats every round trip and never decays. So it starts muted.
    let sameDeviceLogged = false;
    function playbackMutedHere() {
        const same = window.__SAME_DEVICE__ === true;
        if (same && !sameDeviceLogged) {
            sameDeviceLogged = true;
            console.warn('[audio] this page is on the host machine; its playback is muted so the host does not capture it again');
        }
        return same;
    }

    function setMuted(m) {
        state.muted = m;
        if (state.gain) state.gain.gain.value = m ? 0 : 1;
        if (state.audioEl) state.audioEl.muted = m;
    }

    // WebKit only honours resume() inside the running user gesture, and awaiting
    // `ctx.audioWorklet.addModule()` ends that window: construct and resume the context
    // synchronously here and attach the worklet afterwards, or iOS stays suspended all session.
    let preparePromise = null;
    function beginFromGesture() {
        if (state.prepared) {
            // `endSession` suspended the context; this click is the gesture iOS needs to resume it
            if (state.ctx && state.ctx.state === 'suspended') {
                try { const p = state.ctx.resume(); if (p && p.catch) p.catch(note); } catch (_) { note(); }
            }
            return Promise.resolve(true);
        }
        if (preparePromise) return preparePromise;
        initSpeakers();
        const AC = window.AudioContext || window.webkitAudioContext;
        if (!AC) return Promise.resolve(false);
        try {
            state.ctx = new AC({ latencyHint: 'interactive', sampleRate: SR });
        } catch (e) {
            console.warn('[audio] AudioContext construction failed:', e);
            return Promise.resolve(false);
        }
        if (state.ctx.state === 'suspended') {
            try { const p = state.ctx.resume(); if (p && p.catch) p.catch(note); } catch (_) { note(); }
        }
        preparePromise = attachWorklet();
        return preparePromise;
    }

    async function attachWorklet() {
        try {
            await state.ctx.audioWorklet.addModule('/audio-worklet.js');
            state.sab = makeSharedRing();
            state.node = new AudioWorkletNode(state.ctx, 'audio-jitter', {
                numberOfInputs: 0,
                numberOfOutputs: 1,
                outputChannelCount: [CH],
                processorOptions: { capacityFrames: CAP_FRAMES, sab: state.sab },
            });
            state.node.port.onmessage = (e) => {
                if (!e.data) return;
                if (e.data.type === 'stats') {
                    state.stats = e.data;
                    adaptSafetyMargin(e.data);
                } else if (e.data.type === 'ringport-ok') {
                    // the worklet took the far end; an engine that cannot transfer a port into an
                    // AudioWorkletGlobalScope never sends this and keeps the page relay
                    noteRingPortAck();
                } else if (e.data.type === 'mode-mismatch') {
                    // the worklet could not map the shared ring but the worker did: re-init the
                    // worker without the buffer so both sides land on the postMessage ring
                    note();
                    console.warn('[audio] ring mode mismatch; falling back to the postMessage ring');
                    state.sab = null;
                    if (state.worker) state.worker.postMessage({ type: 'init', sab: null, capFrames: CAP_FRAMES });
                }
            };
            // A processor constructor that throws on the render thread does not reject
            // `new AudioWorkletNode`: the node is built and outputs silence. The ring ABI
            // assertions throw that way, so this handler is the only signal.
            state.node.onprocessorerror = () => {
                note();
                state.prepared = false;
                state.prepareFailed = true;
                console.error('[audio] worklet processor failed');
                abandonWebCodecsAudio();
            };
            state.gain = state.ctx.createGain();
            state.gain.gain.value = state.muted ? 0 : 1;
            state.node.connect(state.gain).connect(state.ctx.destination);
            state.prepared = true;
            offerRingPort();
            watchSinkChanges();
            // a chooser pick from the Join gesture, or a sink commanded before the graph existed
            try { const p = applyPendingSink(); if (p && p.catch) p.catch(note); } catch (_) { note(); }
            return true;
        } catch (e) {
            console.warn('[audio] prepare failed:', e);
            state.prepareFailed = true;
            // nothing here can play: close it, or an empty running graph holds the output device
            // (battery and audio-session cost on iOS) for the whole session
            try {
                if (state.ctx) { const p = state.ctx.close(); if (p && p.catch) p.catch(note); }
            } catch (_) { note(); }
            state.ctx = null;
            state.node = null;
            state.gain = null;
            // not a permanent verdict: a module fetch can lose a race with a reconnect, and the
            // host was already told worklet:true, so a memoised failure has nothing to fall back to
            preparePromise = null;
            return false;
        }
    }

    // For callers outside a gesture (a reconfigure). Never constructs a second context: two would
    // fight over the same output.
    function prepare() {
        if (state.prepared) return Promise.resolve(true);
        if (preparePromise) return preparePromise;
        return beginFromGesture();
    }

    async function resume() {
        if (state.ctx && state.ctx.state === 'suspended') {
            try { await state.ctx.resume(); } catch (_) { note(); }
        }
        return !!(state.ctx && state.ctx.state === 'running');
    }

    // Without a SharedArrayBuffer the page relays every packet (200 a second at 5 ms framing), and
    // one long main-thread task is an underrun: up to MAX_TARGET_MS of silence while the buffer
    // re-primes. A MessageChannel takes the page out of the path, but not every engine transfers a
    // port into an AudioWorkletGlobalScope: the worker gets its end only after the worklet acks.
    let ringPortForWorker = null;
    let ringPortAcked = false;
    let ringPortDirect = false;
    function offerRingPort() {
        if (state.sab || ringPortForWorker || !state.node) return;
        if (typeof MessageChannel !== 'function') return;
        let mc;
        try { mc = new MessageChannel(); } catch (_) { note(); return; }
        try {
            state.node.port.postMessage({ type: 'ringport', port: mc.port2 }, [mc.port2]);
        } catch (_) { note(); return; }
        ringPortForWorker = mc.port1;
    }
    function noteRingPortAck() {
        ringPortAcked = true;
        handOffRingPort();
    }
    // runs on the worklet's acknowledgement and again when the worker is built: either can be last
    function handOffRingPort() {
        if (!ringPortAcked || !ringPortForWorker || !state.worker) return;
        const port = ringPortForWorker;
        ringPortForWorker = null;
        try {
            state.worker.postMessage({ type: 'ringport', port: port }, [port]);
            ringPortDirect = true;
        } catch (_) { note(); }
    }
    function releaseRingPort() {
        ringPortForWorker = null;
        ringPortAcked = false;
        ringPortDirect = false;
    }

    function makeSharedRing() {
        if (!sharedMemoryOk()) return null;
        if ((CAP_FRAMES & (CAP_FRAMES - 1)) !== 0) {
            console.error('[audio] ring capacity must be a power of two');
            return null;
        }
        try {
            return new SharedArrayBuffer(CTRL_BYTES + CAP_FRAMES * 4 * 2);
        } catch (_) {
            note();
            return null;
        }
    }

    // proof the worker script ran, not merely that `new Worker` returned
    const WORKER_READY_DEADLINE_MS = 3000;
    let workerReady = false;
    let workerReadyTimer = null;

    function startWorker() {
        if (state.worker) return true;
        let w;
        try {
            w = new Worker('/audio-worker.js');
        } catch (e) {
            console.warn('[audio] decoder worker unavailable:', e);
            return false;
        }
        w.onmessage = (ev) => {
            workerReady = true;
            if (workerReadyTimer) { clearTimeout(workerReadyTimer); workerReadyTimer = null; }
            const d = ev.data;
            if (!d) return;
            if (d.type === 'sync') {
                state.preEnqueueEmaMs = d.preEnqueueMs;
                state.concealed = d.concealed || 0;
                state.redundantUsed = d.redundantUsed || 0;
                state.redundantDiscarded = d.redundantDiscarded || 0;
                maybeUpdateTarget();
            } else if (d.type === 'mode') {
                if (state.node) state.node.port.postMessage({ type: 'mode', shared: !!d.shared });
            } else if (d.type === 'samples') {
                if (state.node) state.node.port.postMessage(d, [d.l.buffer, d.r.buffer]);
            } else if (d.type === 'discontinuity') {
                if (state.node) state.node.port.postMessage({ type: 'discontinuity' });
            } else if (d.type === 'decoder-failed') {
                console.warn('[audio] Opus decode failed repeatedly:', d.message);
                abandonWebCodecsAudio();
            }
        };
        // a Worker whose script 404s, is blocked by CSP, or throws while parsing still constructs
        // and then never posts: an error means the fast path is dead
        w.onerror = (e) => {
            note();
            console.warn('[audio] decoder worker error:', e);
            abandonWebCodecsAudio();
        };
        // the worker answers `init` with `mode`; a deadline on that first reply is the only way to
        // tell a worker that started from one that was built and is dead
        if (workerReadyTimer) clearTimeout(workerReadyTimer);
        workerReady = false;
        workerReadyTimer = setTimeout(function () {
            workerReadyTimer = null;
            if (workerReady || state.worker !== w) return;
            console.warn('[audio] decoder worker never answered in ' + WORKER_READY_DEADLINE_MS + ' ms');
            abandonWebCodecsAudio();
        }, WORKER_READY_DEADLINE_MS);
        w.postMessage({ type: 'init', sab: state.sab, capFrames: CAP_FRAMES });
        state.worker = w;
        // the worker is rebuilt on every reconfigure and stopWorker drops the port, so offer it
        // again here; a no-op on the shared ring and while an offer is outstanding
        offerRingPort();
        handOffRingPort();
        if (state.hostOffsetMs !== null) w.postMessage({ type: 'clockoffset', offsetMs: state.hostOffsetMs });
        return true;
    }

    function stopWorker() {
        releaseRingPort();
        if (workerReadyTimer) { clearTimeout(workerReadyTimer); workerReadyTimer = null; }
        workerReady = false;
        if (!state.worker) return;
        try { state.worker.terminate(); } catch (_) { note(); }
        state.worker = null;
    }

    // The fast path failed after claiming it would work. Detach the DataChannel handler first (it
    // would post into a null worker ~200 times a second), then take the browser's own Opus track if
    // the host sent one; otherwise only a renegotiation, which the page owns, can ask for one.
    function abandonWebCodecsAudio() {
        if (state.decoderFailed) return;
        state.decoderFailed = true;
        if (verified) verified.webcodecsOpus = false;
        if (state.dc) { try { state.dc.onmessage = null; } catch (_) { note(); } }
        stopWorker();
        if (state.fallbackStream) {
            attachFallbackStream(state.fallbackStream);
            return;
        }
        try {
            if (window.SEVideo && SEVideo.onAudioFastLost) SEVideo.onAudioFastLost();
        } catch (_) { note(); }
    }

    async function attachDataChannel(dc) {
        // the host was already told `webcodecsOpus: true`: a failed exit must take the fallback
        // ladder, never a bare return, or the session is silent while reporting the fast path
        if (!(await prepare())) {
            console.warn('[audio] cannot prepare WebCodecs path');
            abandonWebCodecsAudio();
            return;
        }
        if (!startWorker()) { abandonWebCodecsAudio(); return; }
        state.dc = dc;
        dc.binaryType = 'arraybuffer';
        dc.onmessage = (ev) => {
            if (!state.worker) return;
            const buf = ev.data;
            if (!(buf instanceof ArrayBuffer) || buf.byteLength < HDR_BYTES) return;
            state.worker.postMessage(buf, [buf]);
        };
        state.path = 'webcodecs';
        setMuted(playbackMutedHere());
        console.log('[audio] fast path active: Opus over DataChannel → worker → ' +
            (state.sab ? 'shared-memory ring' : 'postMessage ring'));
    }

    // Called from `ontrack`. A host that honours `wantsFallbackTrack` adds a spare recvonly Opus
    // track; attaching it while the WebCodecs path is live would report `netEQ` and play a second,
    // silent element. So remember it and attach only when there is nothing else.
    function noteFallbackStream(stream) {
        state.fallbackStream = stream;
        const claimingFastPath = !!(verified && verified.webcodecsOpus && verified.worklet);
        if (claimingFastPath && !state.decoderFailed) return;
        attachFallbackStream(stream);
    }

    function attachFallbackStream(stream) {
        state.fallbackStream = stream;
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
        setMuted(playbackMutedHere());
        console.log('[audio] fallback path active: standard Opus track (NetEQ)');
    }

    function activePath() {
        return state.path;
    }

    function teardown() {
        stopWorker();
        if (state.dc) { try { state.dc.onmessage = null; } catch (_) { note(); } state.dc = null; }
        if (state.audioEl) { try { state.audioEl.srcObject = null; } catch (_) { note(); } }
        if (state.node) { try { state.node.port.postMessage({ type: 'reset' }); } catch (_) { note(); } }
        state.path = null;
        state.preEnqueueEmaMs = null;
        state.lastTargetMs = null;
        state.residualOffsetMs = 0;
        state.stats = null;
    }

    // Ends the session itself (`teardown` is per peer connection): a page back on its join form
    // holds no microphone, device watch or busy audio device. The context is suspended, not closed:
    // iOS builds one only inside a gesture, and `beginFromGesture` resumes it on the next Connect.
    function endSession() {
        teardown();
        if (speakers.micHold && speakers.appliedSink === SE_EARPIECE) {
            // releasing the hold moves audio off the earpiece: clear the sink so the
            // next session re-acquires it
            speakers.appliedSink = null;
        }
        releaseMicHold();
        stopDeviceWatch();
        speakers.sessionId = '';
        speakers.deviceToken = '';
        speakers.joinId = '';
        // a pending sink belongs to the join that ended; the next Connect asks again
        speakers.pendingSink = null;
        state.fallbackStream = null;
        if (state.audioEl) { try { state.audioEl.pause(); } catch (_) { note(); } }
        if (state.ctx && state.ctx.state === 'running' && typeof state.ctx.suspend === 'function') {
            try { const p = state.ctx.suspend(); if (p && p.catch) p.catch(note); } catch (_) { note(); }
        }
    }

    function getSyncInfo() {
        const st = state.stats;
        return {
            path: state.path,
            // 'shared'      the SharedArrayBuffer ring, worker writes and worklet reads it
            // 'port'        the private ring, samples worker -> worklet over a MessagePort
            // 'postMessage' the private ring, samples worker -> page -> worklet
            transport: state.sab ? 'shared' : (ringPortDirect ? 'port' : 'postMessage'),
            videoDelayMs: state.videoDelayMs,
            hostOffsetMs: state.hostOffsetMs,
            preEnqueueMs: state.preEnqueueEmaMs,
            targetMs: state.lastTargetMs,
            residualOffsetMs: state.residualOffsetMs,
            depthMs: st ? st.depthMs : null,
            underruns: st ? st.underruns : null,
            overruns: st ? st.overruns : null,
            corrections: st ? st.corrections : null,
            concealed: state.concealed + ((st && st.concealed) || 0),
            redundantUsed: state.redundantUsed,
            redundantDiscarded: state.redundantDiscarded,
            safetyMs: state.safetyMs,
            outputLatencyMs: state.outputLatencyMs,
            outputLatencySource: state.outputLatencySource,
            contextState: state.ctx ? state.ctx.state : null,
            decoderFailed: state.decoderFailed,
            labelsUnavailable: !!speakers.labelsUnavailable,
            sinkTier: sinkTier,
            currentSink: speakers.appliedSink,
            // how the outputs were found at the join: action, reason, card answer, per-API results
            outputPlan: lastPlan,
        };
    }

    // the page owns the degradation bitfield; this contributes the audio bits
    function degradeFlags(DEG) {
        let f = 0;
        if (!DEG) return f;
        if (state.decoderFailed || (verified && !verified.webcodecsOpus)) f |= DEG.AUDIO_OPUS_UNSUPPORTED;
        if (state.ctx && state.ctx.state === 'suspended') f |= DEG.AUDIO_CONTEXT_SUSPENDED;
        if (state.outputLatencySource !== 'measured') f |= DEG.AUDIO_OUTPUT_LATENCY_ESTIMATED;
        if (speakers.labelsUnavailable) f |= DEG.AUDIO_OUTPUT_LABELS_UNAVAILABLE;
        // latched: preparePromise is cleared on failure so a later attempt can retry
        if (state.prepareFailed && !state.prepared) f |= DEG.AUDIO_PREPARE_FAILED;
        return f;
    }

    window.SEAudio = {
        detectCapabilities,
        verifyCapabilities,
        beginFromGesture,
        prepare,
        resume,
        attachDataChannel,
        attachFallbackStream,
        noteFallbackStream,
        setMuted,
        isMuted: () => state.muted,
        setVideoDelay,
        setHostClockOffset,
        getSyncInfo,
        degradeFlags,
        sinkTier: () => sinkTier,
        suppressedCount: () => suppressed,
        activePath,
        teardown,
        endSession,
        initSpeakers,
        enumerateBeforeJoin,
        prepareOutputsForJoin,
        startPicker,
        noteOutputChoiceRequest,
        getOutputPlan,
        getDiscovery: () => lastDiscovery,
        CHOOSE_OUTPUT: SE_CHOOSE,
        postOutputs,
        setSpeaker,
        setSpeakerIdentity,
        _enumPromise: null,
    };
})();

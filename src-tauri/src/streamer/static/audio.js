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
        // A property of this build, not of the browser: audio-worker.js can split a
        // FLAG_REDUNDANT body. The host will not set the flag unless it is told this, because
        // a client that cannot split it would decode the two payloads as one packet.
        return { webcodecsOpus, worklet, redundancy: true };
    }

    // The constructor sniff above is not enough on its own. Safari 26 and Firefox 130 have
    // only just shipped AudioDecoder, and the host reads this answer once: claim the fast
    // path and it sends Opus on a DataChannel with no audio track in the SDP at all, so a
    // decoder that then refuses `opus` is permanent silence with no way back.
    // A CELT-only, mono, fullband 20 ms Opus frame: TOC 0xf8 is config 31, stereo bit clear,
    // frame-count code 0. Small enough to inline and enough of a packet to make a decoder
    // produce an AudioData that copyTo can be asked to convert.
    const OPUS_PROBE = new Uint8Array([0xf8, 0xff, 0xfe]);
    const DECODE_PROBE_DEADLINE_MS = 400;

    // isConfigSupported answers about a configuration, not about the call that actually moves
    // the samples. AudioData.copyTo is what deinterleaves into the ring, and an engine that
    // says yes to `opus` and then refuses the f32-planar conversion drops every packet.
    //
    // Deliberately asymmetric about what counts as a refusal: only a decoder that *produced*
    // an AudioData whose copyTo threw, or one that could not be configured at all, demotes.
    // A decoder error or no output within the deadline is inconclusive — the probe packet
    // itself would then be the suspect, and a wrong verdict here disables the fast audio path
    // on every browser at once. The per-packet counter in audio-worker.js is the net for that.
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
        // Says the answer above came from probing a decoder rather than from reading three
        // constructor names off `window`. The host logs it per join, which is what makes a
        // Safari with no AudioDecoder distinguishable from a device that merely failed to
        // build a worklet.
        caps.verified = true;
        // WHEP has no renegotiation verb, so a track absent from the first answer can never
        // be added later — this is the only moment a spare one can exist. Ask for it exactly
        // when it would be a safety net: on the fast path, where the decoder can still fail
        // after saying it would work and where the host otherwise puts no audio in the SDP
        // at all. Off the fast path the host already sends a track it feeds, and asking for a
        // second one would be asking for the same audio twice.
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
        // A/V sync running state
        videoDelayMs: null,     // from the video worker (EMA of display lag)
        hostOffsetMs: null,     // host<->local clock offset, from SEClock via PING/PONG
        preEnqueueEmaMs: null,  // audio capture→enqueue latency, smoothed
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
    // Offered to the host in place of a list when nothing routable enumerates without a
    // permission. Not a device: choosing it asks this page to reveal the real ones.
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
        // Whether the enumeration the host's list was built from carried real device names.
        // `null` until something has enumerated. Posted to the host so a list of "Output 1",
        // "Output 2" is not mistaken for what the operating system calls them.
        outputsLabelled: null,
        sinkWatched: false,
        msd: null,
        sinkEl: null,
        micHold: null,
        micGranted: false,
        // A sink to route once there is a graph to route it through, as
        // `{ id, fromPicker }`. Two things write it: `pickOutput`, whose choice the user made
        // in the browser's own chooser, and `setSpeaker` when the host commands a sink before
        // `attachWorklet` has finished. Only the first may claim `SINK_TIER.selectAudioOutput`,
        // which is why the provenance is carried rather than inferred.
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

    // There is no capability that says "this device has an earpiece" — `navigator.audioSession`
    // exists on iPad too — so some device-shape inference is unavoidable here. What is avoidable
    // is making it the weakest one available: a raw /iPhone/ test loses the Speaker/Earpiece
    // choice on an iPhone with "Request Desktop Website" on, which sends a Macintosh UA. That is
    // the one output control iOS has below 18.4, where HTMLMediaElement.setSinkId arrives.
    //
    // So: iOS by the same test the rest of the file uses (which already handles iPadOS reporting
    // MacIntel), minus the iPads. An iPad is a large touch device — the desktop-UA iPhone still
    // qualifies through `maxTouchPoints` and its screen's short edge, and a real Mac fails
    // detectIOS() outright.
    function detectHasEarpiece() {
        try {
            if (!detectIOS()) return false;
            const ua = navigator.userAgent || '';
            if (/iPhone|iPod/.test(ua)) return true;
            if (/iPad/.test(ua)) return false;
            // Desktop-mode UA. Phones are the narrow ones: every iPhone ever made is at most
            // 440 CSS px on its short edge, every iPad at least 744.
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
            // `setSinkId`, `selectAudioOutput` and `navigator.mediaDevices` are all
            // secure-context only, so on plain HTTP they are simply absent: that is the
            // connection, not the browser, and the page offers HTTPS for it at load.
            speakers.insecure = window.isSecureContext === false;
            speakers.hasSinkApi = speakers.hasElemSink || speakers.hasCtxSink || speakers.hasSelectOut;
            // `false` is what makes the host's sheet say "this device's browser can't switch
            // speakers", so it is sent only when that is true: no sink API of any kind, no
            // earpiece category, and not because the page is on HTTP. Whether a list could be
            // read *yet* (no grant, card skipped, permission denied) is not this question; that
            // device gets the "Choose a speaker on this device…" entry instead.
            speakers.supported = speakers.hasSinkApi || speakers.canCategory || speakers.insecure;
        } catch (_) { note(); }
        if (speakers.hasAudioSession) {
            try { navigator.audioSession.type = 'playback'; } catch (_) { note(); }
        }
    }

    // ── Which speakers this device has, settled before the offer goes out ────────────────
    //
    // The owner's specification (findings/overhaul-new/OWNER-SPEC-audio-discovery.md): on
    // Connect, run every output-discovery API in a fixed order, give each a definite result —
    // `ok`, `unsupported`, `denied`, `empty`, `error`, `timeout` — under its own bounded
    // timeout, and only when none of them produced a routable output raise the microphone card;
    // on Continue, `getUserMedia({audio: true})` once, enumerate again, stop the stream. If that
    // also fails the host is told about exactly one output, the default, `labelled: false`.
    // The whole sequence finishes before the offer is sent, and every result goes to the host in
    // `/audio-outputs` as `discovery` (docs/TELEMETRY.md).
    //
    // What the engines do, measured here (docs/overhaul-new/G06.md, 2026-10-05 addendum): Chrome
    // and Edge 154 with no grant list one `audiooutput` with an empty id and label, which cannot
    // be routed to, so their first join without a microphone grant reaches the card;
    // `permissions.query({name: 'speaker-selection'})` throws `TypeError` there.
    const DISCOVERY_MS = { picker: 30000, permission: 2000, enumerate: 3000, microphone: 30000 };

    function classify(e) {
        const name = e && e.name ? e.name : '';
        if (name === 'NotAllowedError' || name === 'SecurityError') return 'denied';
        if (name === 'NotFoundError') return 'empty';
        return 'error';
    }

    // A call that may throw synchronously as well as reject.
    function attempt(fn) {
        try { return Promise.resolve(fn()); } catch (e) { return Promise.reject(e); }
    }

    // Every discovery call goes through this. A timeout is a definite failure of that API.
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

    // Counted once the result is final, so an expected answer (an unknown permission name's
    // `TypeError`) is not mistaken for a failure nothing else would show.
    function counted(r) {
        if (r.result === 'error' || r.result === 'timeout') note();
        return r;
    }

    // API 1, the browser's own chooser (Firefox 116+, Chromium behind a flag). It needs the
    // Connect click's transient activation, so the submit handler calls this synchronously,
    // before its first await, and only once: a second call after a rejection has no
    // activation left. A dismissed chooser is `denied`.
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
                        // The preferred sink, applied the moment the graph exists.
                        speakers.pendingSink = { id: r.value.deviceId, fromPicker: true };
                    }
                    return counted(r);
                });
        }
        return pickerRun;
    }

    // APIs 2 and 3. An engine that does not know the name throws `TypeError`: `unsupported`.
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

    // API 4. Entries with an empty `deviceId` are Chromium's pre-grant placeholders and cannot
    // be routed to; `default` and `communications` are real ids and are kept.
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

    // The last resort, only from the page's own card: one microphone request, one more
    // enumeration while it is held, then every track stopped.
    function primeWithMicrophone() {
        const md = navigator.mediaDevices;
        if (!md || typeof md.getUserMedia !== 'function') {
            return Promise.resolve({ gum: { label: 'getUserMedia', result: 'unsupported' }, retry: null });
        }
        const asked = attempt(function () { return md.getUserMedia({ audio: true }); });
        return bounded(asked, DISCOVERY_MS.microphone, 'getUserMedia').then(function (r) {
            if (r.result !== 'ok') {
                // A prompt answered after its deadline must not leave the microphone open.
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

    // Routable device entries from the chooser and an enumeration. A labelled list already
    // seen on this page is not replaced by a later unlabelled (or empty) one.
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
    // Run by the Join handler before the offer, with the chooser it started inside the click.
    // `showCard(onContinue, onSkip)` raises the microphone card; it always has Skip, so this
    // never waits on anything the user cannot end, and every call above is bounded.
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
            // Nothing routable from any API. The card only where a grant could change that.
            const md = navigator.mediaDevices;
            if (!md || typeof md.getUserMedia !== 'function') {
                r.getUserMedia = { label: 'getUserMedia', result: 'unsupported' };
                plan.reason = 'no-getUserMedia';
            } else if (r.micPerm.state === 'denied') {
                r.getUserMedia = { label: 'getUserMedia', result: 'denied' };
                plan.reason = 'mic-denied';
            } else if (!speakers.hasSinkApi) {
                // No engine API could route a list even if a grant revealed one.
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

    // Kept for a caller that only wants the cached report (a second join on the same page, or
    // a harness).
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

    // "Choose a speaker on this device…" is no longer offered: discovery now runs in full at
    // every Connect. A host row saved with it before is ignored here and in `setSpeaker`.
    function noteOutputChoiceRequest(sink) {
        return sink === SE_CHOOSE ? 'ignored' : 'cleared';
    }

    function getOutputPlan() { return lastPlan; }

    // The device watch's enumeration (API 8): the same call, bounded the same way.
    async function listOutputs() {
        const r = await enumerateOutputs();
        return r.result === 'ok' ? r.value : [];
    }

    // Unlabelled ids are routable but unreadable, and the host's picker is a list a person
    // chooses from: name them here — the default device by what it is, the rest by position —
    // and tell the host they are generic (`labelled: false`) so it never presents them as what
    // the operating system calls the hardware.
    function noteOutputLabels(devices) {
        const labelled = devices.length ? devices.some(function (d) { return d.label; }) : speakers.canCategory;
        speakers.outputsLabelled = !!labelled;
        speakers.labelsUnavailable = !labelled;
    }

    // What the host is told: the earpiece categories where they apply, every routable device,
    // and, when there is nothing else, exactly one option — the default output.
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
        // Reached only when the host operator chose the earpiece on an engine with no
        // audioSession category (iOS before 16.4), where holding the microphone *is* the routing
        // mechanism. It is the one permission the page can still raise during a session: the
        // operator's choice is the request, the microphone must be held for as long as the
        // earpiece is in use, and an iPhone has no fullscreen or pointer lock for it to drop.
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
            // A refused id may be one that has just been plugged in.
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
        // The poll is the backstop for a browser with no `devicechange` event. Where the event
        // exists it is the mechanism, and a 4 s enumerate-and-POST on the same thread that
        // forwards every 5 ms audio packet on the postMessage ring is a cost paid for nothing.
        // 30 s there is a liveness check, not a device watch.
        const period = speakers.hasDeviceChange ? 30000 : 4000;
        try {
            speakers.poll = setInterval(() => {
                if (document.visibilityState !== 'visible') return;
                reEnumerateAndPost();
            }, period);
        } catch (_) { note(); }
    }

    // The watch belongs to a session: with no join there is nobody to tell about a new
    // speaker, and the next Connect's discovery starts it again.
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

    // Where the audio is *actually* playing, which is not always where the host last asked for
    // it: the user can change it in the browser's own UI (Firefox's per-tab output control, or
    // `selectAudioOutput()` again), and the engine then moves the sink underneath us. Read back
    // rather than assumed, so the host's per-device picker can show what is true.
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

    // `sinkchange` fires on the context whose sink the engine moved; where it does not exist the
    // device poll below is the floor and picks the same change up within its period.
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
        // Enumeration now happens before the join, so there is a window in which there is no
        // session for the host to attach a list to. The identity is set the moment the answer
        // lands and `setSpeakerIdentity` clears the change detection, so the first real post
        // is the one that goes out.
        if (!speakers.sessionId) return;
        // `speakers.supported` is `false` until the probe runs; a post must never carry that.
        initSpeakers();
        try {
            const cache = speakers._cache || { supported: speakers.supported, outputs: buildOutputs([]) };
            const body = JSON.stringify({
                sessionId: speakers.sessionId || '',
                deviceToken: speakers.deviceToken || '',
                supported: cache.supported,
                outputs: cache.outputs,
                selected: speakers.appliedSink || '',
                // What this device is playing on. The client posts the same value in both
                // fields — it only ever knows one — and the separation is the *host's*:
                // `selected` there is what `set_device_audio_output` asked for and the posted
                // `selected` is ignored, so this is the only channel by which a change the user
                // made in the browser's own output control reaches the host at all.
                //
                // Omitted entirely until something has actually applied a sink, because
                // `JSON.stringify` drops an `undefined` value: the host keeps `None`, and the
                // UI can tell "this client never said" from `''`, the device's default output.
                current: speakers.appliedSink === null ? undefined : speakers.appliedSink,
                // False means the names above are this page's own — "Output 1", "Output 2" —
                // because the browser withheld the real ones. Omitted until something has
                // enumerated, so the host can tell "not said" from "generic".
                labelled: speakers.outputsLabelled === null ? undefined : speakers.outputsLabelled,
                // What each discovery API answered at the join (`ok`, `unsupported`, `denied`,
                // `empty`, `error`, `timeout`), keyed by API. docs/TELEMETRY.md.
                discovery: lastDiscovery || undefined,
            });
            // The list is identical on all but a handful of ticks in a session, and the host
            // does the same work whether or not it changed. Re-POSTing it was a request per
            // period per device buying nothing.
            if (!force && body === lastPostedOutputs) return;
            lastPostedOutputs = body;
            // Without a join the host has no session to attach the list to, and it will not
            // guess one from the address.
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

    // Resolves `true` when the audio actually moved, `false` when a mechanism was tried and
    // refused (which is what makes the picker re-offer in index.html correct rather than a
    // guess), and `null` when there is nothing to route yet — the graph is built lazily, so a
    // sink commanded before it exists is remembered and applied by `applyPendingSink`.
    async function setSpeaker(deviceId) {
        initSpeakers();
        const raw = deviceId || '';
        const id = (raw === 'default') ? '' : raw;
        if (speakers.appliedSink === id) return true;
        // The old "Choose a speaker on this device…" entry, still saved on some host rows: a
        // request, not an output, so routing stays where it is.
        if (id === SE_CHOOSE) return true;
        // An id this engine has already refused, still saved on the host (the hardware it named
        // is gone, or it belonged to another browser). Every /reconfig poll carries it, so it
        // is refused once rather than retried — and re-routed — once a second. A device change
        // clears the latch.
        if (speakers.refusedSink === id) return false;

        if (id === SE_EARPIECE || id === SE_LOUDSPEAKER) {
            if (state.path === 'netEQ') { try { await revertSink(); } catch (_) { note(); } }
            else { try { await ensureCategoryBridge(); } catch (_) { note(); } }
            if (id === SE_EARPIECE) {
                if (applySession('play-and-record')) sinkTier = SINK_TIER.audioSession;
                // Only where the platform has no audioSession category is the microphone
                // hold the mechanism that routes to the earpiece; it is not a permission
                // prime and it is released the moment the loudspeaker is chosen again.
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
            // Nothing is playing yet, so nothing has refused: remember it instead of
            // reporting a failure the caller would answer with a picker.
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

            // Its own try, so a NotFoundError on a device that has gone away — or a
            // NotAllowedError — falls through to the element rung below instead of aborting
            // into the shared catch with sinkTier still reporting the sink it did not set.
            if (speakers.hasCtxSink) {
                let ctxSinkOk = false;
                try {
                    await state.ctx.setSinkId(id);
                    ctxSinkOk = true;
                } catch (_) { note(); }
                if (ctxSinkOk) {
                    speakers.appliedSink = id;
                    sinkTier = SINK_TIER.ctxSetSinkId;
                    // A different device has a different buffer depth, so the A/V target that
                    // was right for the old one is not right for this one.
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
                // The sink first, the graph second. Re-routing the graph into the element before
                // the engine had accepted the id left a refused id playing through an element
                // on the default device — and re-assigning `srcObject` restarts the element, so
                // a refused id retried once a second was a restart once a second.
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

    // `selectAudioOutput()` (Firefox 116+, Chromium behind a flag) is the only permission-free
    // output picker in any engine, and the only one that gives a label without a microphone
    // grant. It needs transient activation, so it can only ever come from a gesture the user
    // makes: the Join button's own click, or — after a host-initiated change the engine
    // refused — the next tap on the stage. The page has no control of its own for it, and the
    // stage cannot be one: a live session takes pointer lock on the first gesture and a click
    // there belongs to the host.
    //
    // The Join click lands before there is an AudioContext, so the choice is remembered and
    // `applyPendingSink` routes it the moment the graph exists. The tier is claimed only once
    // the sink actually moved, because a picker that returns a device the engine then refuses
    // to route to is exactly the silent failure `sinkTier` exists to expose.
    function applyPendingSink() {
        const pending = speakers.pendingSink;
        if (!pending) return Promise.resolve(false);
        speakers.pendingSink = null;
        const raw = pending.id;
        const want = (raw === 'default') ? '' : raw;
        return Promise.resolve(setSpeaker(raw)).then(function () {
            if (speakers.appliedSink !== want) return false;
            // Only a sink the user picked in the chooser is tier 4. A host-commanded one that
            // merely arrived before the graph existed took whatever rung `setSpeaker` reached,
            // and `setSpeaker` has already recorded it.
            if (pending.fromPicker) sinkTier = SINK_TIER.selectAudioOutput;
            return true;
        }, function () { note(); return false; });
    }

    function setSpeakerIdentity(sessionId, deviceToken, joinId) {
        speakers.sessionId = sessionId || '';
        speakers.deviceToken = deviceToken || '';
        // The join this list belongs to. Two tabs of one browser enumerate the same speakers
        // and share the sink the host chooses, but each posts under its own session so a list
        // that arrives late cannot land on the other tab's row.
        speakers.joinId = joinId || '';
        // A new session — or a reconfigure that rebuilt one with the same identity — needs the
        // list once regardless of whether it changed, so the change detection in postOutputs
        // cannot swallow the first post of a connection.
        lastPostedOutputs = null;
    }

    // How long a sample takes to get from the graph to the speaker. `outputLatency` is the
    // right answer and arrived in Chrome 102, Firefox 70 and Safari 18.4; treating its
    // absence as zero made the commanded jitter depth too large by exactly that amount, so
    // audio played early — by 3-10 ms on a wired desktop output and 100-200 ms on Bluetooth,
    // which is the difference between lip-sync and obviously wrong.
    //
    // getOutputTimestamp() (Safari 14.1, Firefox 70, Chrome 57) answers the same question
    // indirectly: it reports which sample the device is playing and when, so the distance
    // between that and the graph's clock is the output latency.
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
            // Low-pass it: getOutputTimestamp jitters by a quantum or two and the target it
            // feeds is not worth chasing that.
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

    // The commanded depth is the A/V answer; this is the margin on top of it that keeps the
    // ring from starving on a link that jitters. It grows when the worklet actually runs dry
    // and decays back on a clean one, so a good network converges to no margin at all
    // instead of paying a fixed worst-case buffer forever.
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
        // maybePostStats fires roughly every 256 ms, so eight clean reports is ~2 s.
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

    // Both the audio and the video worker must apply the same clock offset, or their
    // difference — which is what the sync target is built from — moves for no reason.
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

    // A page open on the host machine itself plays into the very mix the host is capturing:
    // the host's loopback takes it back, sends it again, and the result is an echo that repeats
    // every round trip and never decays. The host's own speakers are already playing the audio,
    // so this page's copy adds nothing but that loop — it starts muted.
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

    // Two halves on purpose. WebKit only honours resume() from inside the user gesture that
    // is running when it is called, and `await ctx.audioWorklet.addModule()` ends that
    // window — so the context is constructed and resumed synchronously here, and the
    // worklet is attached afterwards. The old order left iOS suspended for the whole
    // session with the failure swallowed.
    let preparePromise = null;
    function beginFromGesture() {
        if (state.prepared) {
            // `endSession` suspends the context when a join fails or ends; this click is the
            // gesture iOS needs to start it again.
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
                    // The worklet took the far end, so the near end is worth handing to the
                    // worker. Until this arrives — and it never does on an engine that cannot
                    // transfer a port into an AudioWorkletGlobalScope — the samples keep
                    // taking the page relay, which is unchanged.
                    noteRingPortAck();
                } else if (e.data.type === 'mode-mismatch') {
                    // The worklet could not map the shared ring but the worker did, so the
                    // worker must stop writing into it. Re-init it without the buffer and both
                    // sides land on the postMessage ring.
                    note();
                    console.warn('[audio] ring mode mismatch; falling back to the postMessage ring');
                    state.sab = null;
                    if (state.worker) state.worker.postMessage({ type: 'init', sab: null, capFrames: CAP_FRAMES });
                }
            };
            // A processor constructor that throws on the render thread does *not* reject
            // `new AudioWorkletNode`: the node is built and outputs silence. The ring ABI
            // assertions are exactly that kind of throw, so without this the page reported
            // worklet:true, the host answered with no audio track, and the failure was both
            // inaudible and invisible.
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
            // A device chosen from the picker on the Join gesture, or a sink the host commanded
            // before there was a graph to route it through.
            try { const p = applyPendingSink(); if (p && p.catch) p.catch(note); } catch (_) { note(); }
            return true;
        } catch (e) {
            console.warn('[audio] prepare failed:', e);
            state.prepareFailed = true;
            // Nothing here can play. Closing it releases the output device rather than
            // holding an empty running graph open for the whole session — a real battery and
            // audio-session cost on iOS, paid only by the browsers that could not use it.
            try {
                if (state.ctx) { const p = state.ctx.close(); if (p && p.catch) p.catch(note); }
            } catch (_) { note(); }
            state.ctx = null;
            state.node = null;
            state.gain = null;
            // Not a permanent verdict: a module fetch can lose a race with a reconnect, and
            // memoising the failure left the fast path dead for the life of the page — with
            // the host already told worklet:true, so there was nothing to fall back to.
            preparePromise = null;
            return false;
        }
    }

    // Kept for callers that are not inside a gesture (a reconfigure, say): it never
    // constructs a second context, because two would fight over the same output.
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

    // Without a SharedArrayBuffer the decoded samples used to go worker → page → worklet: two
    // postMessage hops and two fresh Float32Arrays per packet, 200 times a second at the host's
    // 5 ms framing, half of them through the main thread that also runs the reconfig fetch, the
    // getStats poll and every pointer event. One long task there is an underrun, and an
    // underrun is up to MAX_TARGET_MS of silence while the buffer re-primes.
    //
    // A MessagePort is transferable, so the two ends can go straight to the worker and the
    // worklet and the page drops out of the path. It is offered rather than assumed: not every
    // engine will transfer a port into an AudioWorkletGlobalScope, so the worklet acknowledges
    // and only then does the worker get the other end. No acknowledgement, no change.
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
    // Runs from the worklet's acknowledgement and again when the worker is built, because
    // either can be the later of the two.
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

    // Proof that the worker script actually ran, not merely that `new Worker` returned.
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
        // A Worker whose script 404s, is blocked by CSP, or throws while parsing is
        // *constructed successfully* and then never posts anything. Logging that and
        // carrying on left dc.onmessage feeding a worker that would never answer, with
        // the client still reporting audioPath 1 while playing silence for the session.
        w.onerror = (e) => {
            note();
            console.warn('[audio] decoder worker error:', e);
            abandonWebCodecsAudio();
        };
        // Nothing above proves the script ran. The worker answers `init` with `mode`,
        // so a deadline on that first reply is the only thing that distinguishes a
        // worker that started from one that was built and is dead.
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
        // attachWorklet runs once per page but the worker is rebuilt on every
        // reconfigure, and stopWorker throws the port away with it — so without this
        // the direct transport existed for the first session only and every later one
        // silently went back to relaying through the page. A no-op on the shared ring
        // and while an offer is already outstanding.
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

    // The decoder said yes to isConfigSupported and then failed anyway. Silence is the worst
    // possible answer, so take every rung there is: the DataChannel handler is detached first
    // (it posted into a worker this is about to null, ~200 uncaught throws a second at the
    // host's 5 ms framing), then the browser's own Opus track if the host put one in the SDP.
    // When it did not — which is precisely the case where the client claimed the fast path —
    // the only route back is a renegotiation that asks for one, which the page owns.
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
        // Both of these used to be a bare `return`: no handler installed, no bit set, and the
        // host already told `webcodecsOpus: true`, so the session played nothing and reported
        // itself on the fast path. Every exit from here now takes the ladder instead.
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

    // What `ontrack` calls. A host that honours `wantsFallbackTrack` puts a recvonly Opus
    // track in the answer as a spare; attaching it while the WebCodecs path is live would
    // report `netEQ` in the stats frame and play a second, silent element beside the worklet.
    // So remember it and attach only when there is nothing else. This is what makes the spare
    // track a safety net rather than a regression.
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

    // `teardown` is per peer connection: a renegotiation calls it and keeps everything else for
    // the next one. This is the end of the session itself — the join failed, the link was lost,
    // the host ended it, or the page is going away — so a page back on its join form holds no
    // microphone, runs no device watch, posts nothing to the host and keeps no audio device
    // busy. The context is suspended rather than closed: iOS builds one only inside a gesture
    // and two would fight over the output, so `beginFromGesture` resumes this one from the next
    // Connect click.
    function endSession() {
        teardown();
        if (speakers.micHold && speakers.appliedSink === SE_EARPIECE) {
            // Releasing the hold is what moves the audio off the earpiece, so the next session
            // must not believe it is still there and skip re-acquiring it.
            speakers.appliedSink = null;
        }
        releaseMicHold();
        stopDeviceWatch();
        speakers.sessionId = '';
        speakers.deviceToken = '';
        speakers.joinId = '';
        // A sink chosen in the browser's chooser at this Connect, or commanded before the graph
        // existed, belongs to the join that failed; the next Connect asks again.
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
            // How the outputs were found at the join: action, reason, the card's answer and the
            // per-API `discovery` results that also go to the host.
            outputPlan: lastPlan,
        };
    }

    // The page owns the degradation bitfield; this reports the audio bits into it so the
    // host sees one number rather than two half-pictures.
    function degradeFlags(DEG) {
        let f = 0;
        if (!DEG) return f;
        if (state.decoderFailed || (verified && !verified.webcodecsOpus)) f |= DEG.AUDIO_OPUS_UNSUPPORTED;
        if (state.ctx && state.ctx.state === 'suspended') f |= DEG.AUDIO_CONTEXT_SUSPENDED;
        if (state.outputLatencySource !== 'measured') f |= DEG.AUDIO_OUTPUT_LATENCY_ESTIMATED;
        if (speakers.labelsUnavailable) f |= DEG.AUDIO_OUTPUT_LABELS_UNAVAILABLE;
        // Latched rather than read off preparePromise, which is cleared on failure so a
        // later attempt is not poisoned by it.
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

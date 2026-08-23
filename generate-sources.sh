#!/usr/bin/env bash
#
# generate-sources.sh — bootstrap reference material for the ScreenExtend system-audio feature.
#
# Clones upstream repos and fetches vendor documentation into ./.sources/ so the implementing
# agent reads real API surfaces and real reference implementations instead of recalling them.
#
# Usage:
#   ./generate-sources.sh              # fetch everything
#   ./generate-sources.sh --repos      # repos only (skip doc fetching)
#   ./generate-sources.sh --docs       # docs only
#   ./generate-sources.sh --clean      # remove .sources/ and exit
#
# Safe to re-run: existing clones are updated, existing docs are re-fetched.
# Nothing here is a build dependency. .sources/ is reference material only and is gitignored.

set -euo pipefail

SOURCES_DIR="${SOURCES_DIR:-.sources}"
DO_REPOS=1
DO_DOCS=1

for arg in "$@"; do
  case "$arg" in
    --repos) DO_DOCS=0 ;;
    --docs)  DO_REPOS=0 ;;
    --clean) echo "Removing ${SOURCES_DIR}/"; rm -rf "${SOURCES_DIR}"; exit 0 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

# ─────────────────────────────────────────────────────────────────────────────
# Helpers
# ─────────────────────────────────────────────────────────────────────────────

C_OK="\033[32m"; C_WARN="\033[33m"; C_ERR="\033[31m"; C_DIM="\033[2m"; C_OFF="\033[0m"
ok()   { printf "${C_OK}  ok${C_OFF}   %s\n" "$*"; }
warn() { printf "${C_WARN} warn${C_OFF}   %s\n" "$*"; }
fail() { printf "${C_ERR} fail${C_OFF}   %s\n" "$*"; }
section() { printf "\n${C_DIM}── %s ─────────────────────────────────────────${C_OFF}\n" "$*"; }

FAILURES=0
note_failure() { FAILURES=$((FAILURES + 1)); }

require() {
  command -v "$1" >/dev/null 2>&1 || { fail "'$1' is required but not installed."; exit 1; }
}

# clone <name> <url> [ref]
#   Shallow clone. Updates in place if already present.
clone() {
  local name="$1" url="$2" ref="${3:-}"
  local dest="${SOURCES_DIR}/repos/${name}"
  if [ -d "${dest}/.git" ]; then
    if git -C "${dest}" fetch --depth 1 origin >/dev/null 2>&1 \
      && git -C "${dest}" reset --hard origin/HEAD >/dev/null 2>&1; then
      ok "${name} (updated)"
    else
      warn "${name} (already present; update failed — using existing copy)"
    fi
    return 0
  fi
  if [ -n "${ref}" ]; then
    git clone --depth 1 --branch "${ref}" --quiet "${url}" "${dest}" 2>/dev/null \
      && { ok "${name} @ ${ref}"; return 0; }
  else
    git clone --depth 1 --quiet "${url}" "${dest}" 2>/dev/null \
      && { ok "${name}"; return 0; }
  fi
  fail "${name} — clone failed (${url})"; note_failure; return 0
}

# sparse_clone <name> <url> <path>...
#   For very large repos: fetch only the listed subtrees.
sparse_clone() {
  local name="$1" url="$2"; shift 2
  local dest="${SOURCES_DIR}/repos/${name}"
  if [ -d "${dest}/.git" ]; then ok "${name} (already present)"; return 0; fi
  {
    git clone --depth 1 --filter=blob:none --sparse --quiet "${url}" "${dest}" \
      && git -C "${dest}" sparse-checkout set "$@"
  } >/dev/null 2>&1 \
    && { ok "${name} (sparse: $*)"; return 0; }
  fail "${name} — sparse clone failed (${url})"; note_failure; return 0
}

# fetch <category> <filename> <url>
fetch() {
  local cat="$1" name="$2" url="$3"
  local dest="${SOURCES_DIR}/docs/${cat}"
  mkdir -p "${dest}"
  if curl -fsSL --retry 2 --max-time 45 \
       -A "Mozilla/5.0 (compatible; ScreenExtend-source-bootstrap)" \
       "${url}" -o "${dest}/${name}" 2>/dev/null; then
    ok "${cat}/${name}"
  else
    warn "${cat}/${name} — fetch failed, read online: ${url}"
    printf 'FETCH FAILED — read this online:\n%s\n' "${url}" > "${dest}/${name}.MISSING.txt"
    note_failure
  fi
}

require git
require curl

mkdir -p "${SOURCES_DIR}/repos" "${SOURCES_DIR}/docs"

# ─────────────────────────────────────────────────────────────────────────────
# Repos
# ─────────────────────────────────────────────────────────────────────────────

if [ "${DO_REPOS}" -eq 1 ]; then

section "WASAPI capture — reference implementations"

# OBS's WASAPI source. The single best real-world reference for loopback capture:
# handles device loss/default-device changes, format negotiation, the silent-endpoint
# problem, and process loopback gated on build number. Read win-wasapi.c first.
sparse_clone obs-studio https://github.com/obsproject/obs-studio.git \
  plugins/win-wasapi libobs/media-io libobs/util

# Microsoft's own samples. ApplicationLoopback demonstrates the process-loopback API
# (build 20348+, OUT of scope for our baseline — read it to understand *why* it's out).
# WASAPICaptureSharedEventDriven is the event-driven capture pattern we do want.
sparse_clone windows-classic-samples https://github.com/microsoft/Windows-classic-samples.git \
  Samples/ApplicationLoopback \
  Samples/WASAPICaptureSharedEventDriven \
  Samples/WASAPIRenderSharedEventDriven \
  Samples/AudioEndpointVolume

# Rust WASAPI wrappers — idiomatic bindings over the same COM surface we're targeting.
clone wasapi-rs https://github.com/HEnquist/wasapi-rs.git
clone cpal       https://github.com/RustAudio/cpal.git

# The windows-rs crate: authoritative for which Win32_Media_Audio types/flags are exposed
# and what feature gates they sit behind.
sparse_clone windows-rs https://github.com/microsoft/windows-rs.git \
  crates/libs/windows/src/Windows/Win32/Media/Audio \
  crates/libs/windows/src/Windows/Win32/System/Threading

section "Opus — encoder"

# libopus source. Read include/opus.h and include/opus_defines.h for the exact
# OPUS_SET_* control macro values we must reproduce in a hand-written FFI shim.
clone opus https://github.com/xiph/opus.git

# Existing Rust bindings — reference for FFI shape and build config, not a dependency.
# The repo hand-writes its *_sys layers (see x264_sys.rs, nvenc_sys), so we follow that.
clone audiopus_sys https://github.com/lakelezz/audiopus_sys.git
clone opus-rs       https://github.com/SpaceManiac/opus-rs.git

section "WebRTC transport"

# webrtc-rs is already a dependency (0.17). Clone it to read the Opus payloader,
# TrackLocalStaticSample, and the DataChannel reliability/ordering options directly.
clone webrtc-rs https://github.com/webrtc-rs/webrtc.git

section "Client-side decode and playback"

# WebCodecs spec + explainer: AudioDecoder config, EncodedAudioChunk, AudioData.
clone webcodecs https://github.com/w3c/webcodecs.git

# Ring buffers between a worker and an AudioWorklet over SharedArrayBuffer.
# padenot is the Firefox Web Audio implementer; this is the canonical lock-free pattern.
clone ringbuf.js https://github.com/padenot/ringbuf.js.git

# Google's AudioWorklet examples, including the free-queue / SAB patterns.
sparse_clone web-audio-samples https://github.com/GoogleChromeLabs/web-audio-samples.git \
  src/audio-worklet

fi

# ─────────────────────────────────────────────────────────────────────────────
# Documentation
# ─────────────────────────────────────────────────────────────────────────────

if [ "${DO_DOCS}" -eq 1 ]; then

section "Microsoft — WASAPI / Core Audio"

MSDOCS="https://raw.githubusercontent.com/MicrosoftDocs/win32/docs/desktop-src"
fetch wasapi loopback-recording.md          "${MSDOCS}/CoreAudio/loopback-recording.md"
fetch wasapi capturing-a-stream.md          "${MSDOCS}/CoreAudio/capturing-a-stream.md"
fetch wasapi rendering-a-stream.md          "${MSDOCS}/CoreAudio/rendering-a-stream.md"
fetch wasapi about-wasapi.md                "${MSDOCS}/CoreAudio/wasapi.md"
fetch wasapi device-events.md               "${MSDOCS}/CoreAudio/device-events.md"
fetch wasapi audio-session-events.md        "${MSDOCS}/CoreAudio/audio-session-events.md"
fetch wasapi stream-latency.md              "${MSDOCS}/CoreAudio/reducing-latency-in-audio-applications.md"
fetch wasapi exclusive-mode-streams.md      "${MSDOCS}/CoreAudio/exclusive-mode-streams.md"

# Live pages (structured reference for the interfaces we call directly).
fetch wasapi IAudioClient3.html      "https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nn-audioclient-iaudioclient3"
fetch wasapi InitializeSharedAudioStream.html "https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient3-initializesharedaudiostream"
fetch wasapi GetSharedModeEnginePeriod.html   "https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient3-getsharedmodeengineperiod"
fetch wasapi AUDCLNT_STREAMFLAGS.html "https://learn.microsoft.com/en-us/windows/win32/coreaudio/audclnt-streamflags-xxx-constants"
fetch wasapi IAudioCaptureClient_GetBuffer.html "https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudiocaptureclient-getbuffer"
fetch wasapi AUDCLNT_BUFFERFLAGS.html "https://learn.microsoft.com/en-us/windows/win32/api/audioclient/ne-audioclient-_audclnt_bufferflags"
fetch wasapi low-latency-audio.html   "https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/low-latency-audio"
fetch wasapi mmcss-multimedia-class.html "https://learn.microsoft.com/en-us/windows/win32/procthread/multimedia-class-scheduler-service"
fetch wasapi AvSetMmThreadCharacteristics.html "https://learn.microsoft.com/en-us/windows/win32/api/avrt/nf-avrt-avsetmmthreadcharacteristicsw"
fetch wasapi IMMNotificationClient.html "https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-immnotificationclient"
fetch wasapi ActivateAudioInterfaceAsync.html "https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-activateaudiointerfaceasync"

section "Opus"

fetch opus rfc6716-opus-codec.txt      "https://www.rfc-editor.org/rfc/rfc6716.txt"
fetch opus rfc7587-rtp-payload.txt     "https://www.rfc-editor.org/rfc/rfc7587.txt"
fetch opus rfc8251-opus-updates.txt    "https://www.rfc-editor.org/rfc/rfc8251.txt"
fetch opus opus-api-encoder.html       "https://opus-codec.org/docs/opus_api-1.5/group__opus__encoder.html"
fetch opus opus-api-ctls.html          "https://opus-codec.org/docs/opus_api-1.5/group__opus__encoderctls.html"
fetch opus opus-api-genericctls.html   "https://opus-codec.org/docs/opus_api-1.5/group__opus__genericctls.html"

section "WebCodecs / Web Audio / browser platform"

MDN="https://raw.githubusercontent.com/mdn/content/main/files/en-us/web/api"
fetch web audiodecoder.md          "${MDN}/audiodecoder/index.md"
fetch web audiodecoder-configure.md "${MDN}/audiodecoder/configure/index.md"
fetch web audiodata.md             "${MDN}/audiodata/index.md"
fetch web encodedaudiochunk.md     "${MDN}/encodedaudiochunk/index.md"
fetch web audioworklet.md          "${MDN}/audioworklet/index.md"
fetch web audioworkletprocessor.md "${MDN}/audioworkletprocessor/index.md"
fetch web audiocontext.md          "${MDN}/audiocontext/index.md"
fetch web rtcdatachannel.md        "${MDN}/rtcdatachannel/index.md"
fetch web sharedarraybuffer.md     "${MDN}/../javascript/reference/global_objects/sharedarraybuffer/index.md"

fetch web webcodecs-codec-registry.html "https://www.w3.org/TR/webcodecs-opus-codec-registration/"
fetch web cross-origin-isolation.html   "https://web.dev/articles/coop-coep"
fetch web audio-output-latency.html     "https://developer.chrome.com/blog/audio-worklet"

section "A/V sync + clocks"

fetch sync qpc-acquiring-timestamps.html "https://learn.microsoft.com/en-us/windows/win32/sysinfo/acquiring-high-resolution-time-stamps"
fetch sync IAudioClock.html              "https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nn-audioclient-iaudioclock"
fetch sync rfc3550-rtp.txt               "https://www.rfc-editor.org/rfc/rfc3550.txt"

fi

# ─────────────────────────────────────────────────────────────────────────────
# Index + gitignore
# ─────────────────────────────────────────────────────────────────────────────

cat > "${SOURCES_DIR}/README.md" <<'INDEX'
# .sources — reference material (not a build dependency)

Generated by `generate-sources.sh`. Nothing in here is compiled, linked, or shipped.
It exists so the implementing agent reads real API surfaces rather than recalling them.

## Read in this order

**1. Capture — how loopback actually behaves**
- `repos/obs-studio/plugins/win-wasapi/win-wasapi.c` — the most battle-tested loopback
  implementation in the open. Note specifically how it handles: default-device changes,
  device loss (`AUDCLNT_E_DEVICE_INVALIDATED`), format negotiation, and the idle-endpoint
  silence problem.
- `repos/windows-classic-samples/Samples/WASAPICaptureSharedEventDriven` — the event-driven
  pattern.
- `repos/windows-classic-samples/Samples/ApplicationLoopback` — the *process* loopback API.
  Read it to understand why it is out of scope for our build baseline (needs 20348+).
- `docs/wasapi/loopback-recording.md`, `docs/wasapi/low-latency-audio.html`
- `repos/wasapi-rs/src/api.rs` — the same COM calls, in Rust.

**2. Encode**
- `repos/opus/include/opus.h` and `include/opus_defines.h` — the exact control macro values
  the hand-written FFI shim must reproduce. Do not guess these; read them.
- `docs/opus/opus-api-ctls.html` — semantics of each `OPUS_SET_*`.
- `docs/opus/rfc7587-rtp-payload.txt` — the fmtp parameters browsers negotiate.

**3. Transport**
- `repos/webrtc-rs/webrtc/src/track/track_local/track_local_static_sample.rs`
- `repos/webrtc-rs/webrtc/src/data_channel/` — ordering and retransmit configuration.
- `repos/webrtc-rs/rtp/src/codecs/opus/` — the Opus payloader.

**4. Client decode + playback**
- `docs/web/audiodecoder-configure.md` and `docs/web/encodedaudiochunk.md`
- `repos/ringbuf.js/js/` — lock-free SAB ring buffer between worker and AudioWorklet.
- `repos/web-audio-samples/src/audio-worklet/` — worklet plumbing patterns.
- `docs/web/cross-origin-isolation.html` — SAB requires COOP/COEP **and** a secure context.

**5. Sync**
- `docs/sync/qpc-acquiring-timestamps.html`, `docs/sync/IAudioClock.html`

## Caveat

Some of these are recent upstream `HEAD`, not the versions ScreenExtend pins. Where a
reference disagrees with the crate version in `src-tauri/Cargo.toml`, **the pinned crate
wins** — check the local `~/.cargo/registry` source or `cargo doc --open` before porting
an API call verbatim.
INDEX

if ! grep -qs '^\.sources/' .gitignore 2>/dev/null; then
  printf '\n# Reference material fetched by generate-sources.sh (not a build dependency)\n.sources/\n' >> .gitignore
  ok "added .sources/ to .gitignore"
fi

section "Done"
du_out="$(du -sh "${SOURCES_DIR}" 2>/dev/null | cut -f1 || echo '?')"
echo "  ${SOURCES_DIR}/  (${du_out})"
echo "  Index: ${SOURCES_DIR}/README.md"

if [ "${FAILURES}" -gt 0 ]; then
  warn "${FAILURES} item(s) failed to fetch — see the .MISSING.txt files, or read those URLs online."
  echo "  This is not fatal. Re-run to retry, or continue with what was fetched."
fi

echo
echo "Next: read ${SOURCES_DIR}/README.md, then follow PRD.md."

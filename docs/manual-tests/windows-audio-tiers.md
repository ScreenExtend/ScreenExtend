# Manual tests — Windows audio tiers

Four things the Windows dev box (Windows 11 Home 10.0.26200) cannot settle on its own. Each
one has a branch of shipped code behind it that is currently `COMPILE-CHECKED` rather than
`VERIFIED`, and each is cheap once the hardware exists.

Every measurement below is `ScreenExtend probe --audio [--process-loopback] [--duration N]`,
which prints packet rate, subscriber inter-arrival, capture→subscriber latency, the signed
skew of `capture_ns` against the host clock, and the full `AudioDiagnostics` summary. See
`docs/TELEMETRY.md`.

## 1. The low-latency engine period, on a driver that has one

**Why.** The silent render companion asks the engine for its minimum period and the loopback
capture stream inherits it (`windows_utils/audio/silence.rs`). On this box the driver reports
`default = fundamental = min = max = 480 frames (10.000 ms)`, so the probe correctly declines
and the request branch has never executed. Microsoft documents the in-box **High Definition
Audio Device** driver as supporting **128–480 frames**, which is the cheapest way to reach it.

**How.**

1. `ScreenExtend probe --audio --duration 15` and record the `endpoint engine periods` line and
   the `wake` histogram. Baseline: `default=480fr … min=480fr`, wake p50 9.44 ms.
2. Device Manager → *Sound, video and game controllers* → the audio device → **Update driver**
   → *Browse my computer* → *Let me pick from a list* → **High Definition Audio Device** →
   reboot.
3. Re-run. Expect `min=128fr (2.667ms)`, a log line reading `companion pulled the endpoint
   engine period to 128 frames`, and a wake histogram around **2.7 ms** instead of 9.4 ms.
4. Then check the regression risks the change is most likely to cause, all in the same summary
   line: `discontinuity`, `silent` and `dc_drops` must not increase, and `starvations` must
   stay 0. Microsoft warns that small buffers cost power — at 2.67 ms the capture thread wakes
   ~375 times/s instead of ~100 — so sample the process's CPU before and after.
5. Restore the OEM driver afterwards.

**Expected result if it works:** −5 to −7.3 ms off the capture period, which is the largest
single controllable term in the Windows audio budget.

## 2. The process-loopback probe refusing on an older Windows

**Why.** `windows_utils/audio/process_loopback.rs` is probed by attempting the activation, not
by a build number, and the refusal path has never run: this box is 26200 and the activation
succeeds. A retail **Windows 10 19045** host (below build 20348) is what exercises it.

**How.** On the 19045 host, `ScreenExtend probe --audio`. The log must read
`process loopback unavailable (…); endpoint loopback is the only tier` and audio must work
exactly as before — the refusal is expected and must be invisible to the user.

Then force the endpoint tier to fail (open the default endpoint in exclusive mode from another
application) and confirm the fallback message appears and audio recovers on a machine where
tier 2 *is* available.

## 3. `OPUS_SET_PREDICTION_DISABLED`

**Why.** `OpusEncoderConfig::disable_prediction` defaults to `false`. Prediction-disabled makes
every frame nearly independent, which is what a zero-retransmit unordered transport wants, but
it costs bitrate at fixed quality. That is a listening decision, not a measurement, so it was
not landed blind.

**How.** Flip the default, play a transient-heavy source (percussion, applause) plus speech,
and A/B against the current build over a real Wi-Fi link with 2 % and 5 % induced loss
(`clumsy`). Watch `SEAudio.getSyncInfo()` for `underruns` and the commanded `targetMs`. Keep it
only if the loss-robustness gain is audible and the clean-link quality loss is not.

## 4. Constrained VBR's effect on packet size

**Why.** `OPUS_SET_VBR_CONSTRAINT` went 0 → 1 on the argument that unconstrained VBR spikes
packet size on transients, which becomes arrival jitter, which the client answers by growing
its buffer. The setting is confirmed in force by reading it back, but the *distribution* is
not: an idle host encodes 3-byte silence frames, so this box cannot see the tail.

**How.** With real audio playing, run `probe --audio --duration 60` and compare `payload avg`
and `max` against a build with the constraint back off. Expect the maximum to come down toward
the nominal ~118 bytes while the average barely moves. Then a 10-minute client session on real
Wi-Fi, watching whether `getSyncInfo().targetMs` settles lower.

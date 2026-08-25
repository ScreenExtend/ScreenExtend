// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).

#include "device.hpp"
#include "branding.hpp"
#include "volume_control.hpp"

#include <aspl/Context.hpp>
#include <aspl/Device.hpp>
#include <aspl/Stream.hpp>

#include <cstring>

namespace se_audio {

CaptureHandler::CaptureHandler(std::shared_ptr<ShmRing> shm)
    : shm_(std::move(shm))
    , loopback_(size_t(kLoopbackFrames) * kChannels, 0.0f)
    , loopbackMask_(kLoopbackFrames - 1)
{
}

void CaptureHandler::OnProcessMixedOutput(const std::shared_ptr<aspl::Stream>& /*stream*/,
    Float64 /*zeroTimestamp*/,
    Float64 timestamp,
    Float32* frames,
    UInt32 frameCount,
    UInt32 channelCount)
{
    if (frames == nullptr || frameCount == 0 || channelCount == 0) {
        return;
    }

    // Downmix defensively to stereo indices; the device advertises 2ch, so channelCount is 2, but
    // stay robust if the HAL hands us a different count.
    const uint32_t ch = channelCount;
    const uint32_t sampleCount = frameCount * ch;

    // 1. Shared-memory transport (fast path). Full-scale, pre-gain (PRD §6.2). RT-safe.
    if (ch == kChannels) {
        shm_->Write(frames, sampleCount);
    } else {
        // Non-stereo mix: forward the interleaved block as-is; the shm header still says stereo,
        // so the host treats a mismatched channelCount as a format change via the generation
        // counter. In practice the device is fixed at stereo and this branch is unreachable.
        shm_->Write(frames, sampleCount);
    }

    // 2. Internal loopback ring for the HAL-input fallback, indexed by absolute sample-time so the
    //    input stream can read the same frames at its own read timestamp.
    const uint64_t base = static_cast<uint64_t>(timestamp);
    for (uint32_t f = 0; f < frameCount; ++f) {
        const uint32_t idx = static_cast<uint32_t>((base + f) & loopbackMask_) * kChannels;
        const uint32_t src = f * ch;
        loopback_[idx + 0] = frames[src + 0];
        loopback_[idx + 1] = (ch >= 2) ? frames[src + 1] : frames[src + 0];
    }
    // Deliberately NOT calling stream->ApplyProcessing(): the captured/looped audio must stay
    // pre-gain. The device's output goes nowhere real, so skipping processing has no downside.
}

void CaptureHandler::OnReadClientInput(const std::shared_ptr<aspl::Client>& /*client*/,
    const std::shared_ptr<aspl::Stream>& /*stream*/,
    Float64 /*zeroTimestamp*/,
    Float64 timestamp,
    void* bytes,
    UInt32 bytesCount)
{
    auto* out = static_cast<float*>(bytes);
    const uint32_t outSamples = bytesCount / sizeof(float);
    const uint32_t frameCount = outSamples / kChannels;
    const uint64_t base = static_cast<uint64_t>(timestamp);

    for (uint32_t f = 0; f < frameCount; ++f) {
        const uint32_t idx = static_cast<uint32_t>((base + f) & loopbackMask_) * kChannels;
        out[f * kChannels + 0] = loopback_[idx + 0];
        out[f * kChannels + 1] = loopback_[idx + 1];
    }
    // Any trailing bytes (non-multiple of a frame) are left as the caller supplied; HAL always
    // requests whole frames, so this does not occur in practice.
}

std::shared_ptr<aspl::Device> BuildDevice(const std::shared_ptr<aspl::Context>& context,
    const std::shared_ptr<CaptureHandler>& handler)
{
    aspl::DeviceParameters params;
    params.Name = branding::kDeviceName;
    params.Manufacturer = branding::kManufacturer;
    params.DeviceUID = branding::kDeviceUID;
    params.ModelUID = branding::kModelUID;
    params.SampleRate = 48000;   // Opus-native; nothing in the chain resamples (PRD §5.3).
    params.ChannelCount = 2;     // one use case: stereo. No 16/64-channel variants (PRD §8.1).
    params.Latency = 0;          // report zero added latency (PRD §5.3).
    params.SafetyOffset = 0;
    params.EnableMixing = true;  // receive the mixed system output in OnProcessMixedOutput.
    params.CanBeDefault = true;
    params.CanBeDefaultForSystemSounds = true;

    auto device = std::make_shared<aspl::Device>(context, params);

    // Output stream WITH volume + mute controls on the output scope — this is what re-enables the
    // macOS volume keys and menu-bar slider while we are the default output (PRD §6.2, layer 1).
    device->AddStreamWithControlsAsync(aspl::Direction::Output);

    // Input stream (loopback) so the host can read the capture via a plain HAL IOProc when the
    // shared-memory transport is unavailable (PRD §5.2a fallback). No controls needed on input.
    device->AddStreamAsync(aspl::Direction::Input);

    // Sensible initial volume/mute state + trace hooks (host-side is the real gain stage).
    ConfigureVolumeAndMute(device);

    device->SetIOHandler(handler);
    device->SetControlHandler(handler);

    return device;
}

} // namespace se_audio

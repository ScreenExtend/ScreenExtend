// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).

#include "device.hpp"
#include "branding.hpp"
#include "log.hpp"
#include "volume_control.hpp"

#include <aspl/Context.hpp>
#include <aspl/Device.hpp>
#include <aspl/Stream.hpp>

#include <algorithm>
#include <cstring>
#include <mach/mach_time.h>

namespace se_audio {

namespace {

constexpr float kM3dB = 0.70710678f;

/// BS.775 downmix of one frame to (L, R); mirror of
/// `macos_utils/audio/format.rs::downmix_channels` and its Windows sibling.
inline void DownmixFrame(const Float32* ch, uint32_t n, float& l, float& r)
{
    switch (n) {
        case 0:
            l = 0.0f;
            r = 0.0f;
            return;
        case 1:
            l = ch[0];
            r = ch[0];
            return;
        case 2:
            l = ch[0];
            r = ch[1];
            return;
        case 3: { // FL, FR, FC
            const float c = ch[2] * kM3dB;
            l = ch[0] + c;
            r = ch[1] + c;
            return;
        }
        case 4: // quad: FL, FR, BL, BR
            l = ch[0] + ch[2] * kM3dB;
            r = ch[1] + ch[3] * kM3dB;
            return;
        case 6: { // 5.1: FL, FR, FC, LFE, BL, BR (LFE dropped)
            const float c = ch[2] * kM3dB;
            l = ch[0] + c + ch[4] * kM3dB;
            r = ch[1] + c + ch[5] * kM3dB;
            return;
        }
        case 8: { // 7.1: FL, FR, FC, LFE, BL, BR, SL, SR (LFE dropped)
            const float c = ch[2] * kM3dB;
            l = ch[0] + c + (ch[4] + ch[6]) * kM3dB;
            r = ch[1] + c + (ch[5] + ch[7]) * kM3dB;
            return;
        }
        default: { // unknown layout: average into both, which cannot clip
            float acc = 0.0f;
            for (uint32_t c = 0; c < n; ++c) {
                acc += ch[c];
            }
            l = acc / float(n);
            r = l;
            return;
        }
    }
}

inline float Clamp1(float v)
{
    return std::min(1.0f, std::max(-1.0f, v));
}

} // namespace

CaptureHandler::CaptureHandler(std::shared_ptr<ShmRing> shm)
    : shm_(std::move(shm))
    , loopback_(size_t(kLoopbackFrames) * kChannels, 0.0f)
    , loopbackMask_(kLoopbackFrames - 1)
    , downmix_(size_t(kMaxFramesPerCallback) * kChannels, 0.0f)
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

    // A commpage read, not a syscall, so it is safe on this thread; it is the only record of when
    // the mix existed, and everything downstream dates its samples from it.
    const uint64_t hostTime = ::mach_absolute_time();

    const uint32_t ch = channelCount;
    if (ch == kChannels) {
        shm_->Write(frames, frameCount * ch, hostTime);
    } else {
        // The ring's contract is interleaved stereo — the header says `channels = 2` and the
        // reader deinterleaves on that basis — so anything else is downmixed here, into scratch
        // allocated in the constructor. Writing the raw N-channel mix instead would give the
        // reader channel smear and a writePos advancing N/2 times too fast.
        const uint32_t n = std::min<uint32_t>(frameCount, kMaxFramesPerCallback);
        for (uint32_t f = 0; f < n; ++f) {
            float l = 0.0f;
            float r = 0.0f;
            DownmixFrame(frames + size_t(f) * ch, ch, l, r);
            downmix_[size_t(f) * kChannels + 0] = Clamp1(l);
            downmix_[size_t(f) * kChannels + 1] = Clamp1(r);
        }
        shm_->Write(downmix_.data(), n * kChannels, hostTime);
    }

    // loopback ring for the HAL-input fallback, indexed by absolute sample-time
    const uint64_t base = static_cast<uint64_t>(timestamp);
    for (uint32_t f = 0; f < frameCount; ++f) {
        const uint32_t idx = static_cast<uint32_t>((base + f) & loopbackMask_) * kChannels;
        const uint32_t src = f * ch;
        loopback_[idx + 0] = frames[src + 0];
        loopback_[idx + 1] = (ch >= 2) ? frames[src + 1] : frames[src + 0];
    }
}

void CaptureHandler::OnReadClientInput(const std::shared_ptr<aspl::Client>& /*client*/,
    const std::shared_ptr<aspl::Stream>& /*stream*/,
    Float64 /*zeroTimestamp*/,
    Float64 timestamp,
    void* bytes,
    UInt32 bytesCount)
{
    if (bytes == nullptr || bytesCount == 0) {
        return;
    }
    auto* out = static_cast<float*>(bytes);
    const uint32_t outSamples = bytesCount / sizeof(float);
    const uint32_t frameCount = outSamples / kChannels;

    // A negative or NaN sample time would index the ring at an arbitrary masked offset; silence is
    // the only honest answer.
    if (!(timestamp >= 0.0)) {
        std::memset(bytes, 0, bytesCount);
        return;
    }
    const uint64_t base = static_cast<uint64_t>(timestamp);

    for (uint32_t f = 0; f < frameCount; ++f) {
        const uint32_t idx = static_cast<uint32_t>((base + f) & loopbackMask_) * kChannels;
        out[f * kChannels + 0] = loopback_[idx + 0];
        out[f * kChannels + 1] = loopback_[idx + 1];
    }
    // `bytesCount` need not be a whole number of frames, and the HAL does not promise a zeroed
    // buffer, so the tail would otherwise replay whatever was in it as a per-cycle click.
    const uint32_t tail = outSamples - frameCount * kChannels;
    if (tail != 0) {
        std::memset(out + frameCount * kChannels, 0, size_t(tail) * sizeof(float));
    }
}

std::shared_ptr<aspl::Device> BuildDevice(const std::shared_ptr<aspl::Context>& context,
    const std::shared_ptr<CaptureHandler>& handler)
{
    aspl::DeviceParameters params;
    params.Name = branding::kDeviceName;
    params.Manufacturer = branding::kManufacturer;
    params.DeviceUID = branding::kDeviceUID;
    params.ModelUID = branding::kModelUID;
    params.SampleRate = kSampleRate; // Opus-native; nothing in the chain resamples
    params.ChannelCount = kChannelCount;
    params.Latency = 0;
    params.SafetyOffset = 0;
    params.EnableMixing = true; // deliver the mixed system output to OnProcessMixedOutput
    params.CanBeDefault = true;
    params.CanBeDefaultForSystemSounds = true;

    auto device = std::make_shared<aspl::Device>(context, params);

    device->AddStreamWithControlsAsync(aspl::Direction::Output);
    device->AddStreamAsync(aspl::Direction::Input);

    ConfigureVolumeAndMute(device);

    device->SetIOHandler(handler);
    device->SetControlHandler(handler);

    SE_LOG("device built: %{public}s (%{public}s) %{public}u Hz x%{public}u",
        branding::kDeviceName,
        branding::kDeviceUID,
        kSampleRate,
        kChannelCount);

    return device;
}

} // namespace se_audio

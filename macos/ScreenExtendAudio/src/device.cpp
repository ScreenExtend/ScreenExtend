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

    const uint32_t ch = channelCount;
    const uint32_t sampleCount = frameCount * ch;

    // fast path: full-scale, pre-gain, RT-safe
    if (ch == kChannels) {
        shm_->Write(frames, sampleCount);
    } else {
        shm_->Write(frames, sampleCount);
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
    auto* out = static_cast<float*>(bytes);
    const uint32_t outSamples = bytesCount / sizeof(float);
    const uint32_t frameCount = outSamples / kChannels;
    const uint64_t base = static_cast<uint64_t>(timestamp);

    for (uint32_t f = 0; f < frameCount; ++f) {
        const uint32_t idx = static_cast<uint32_t>((base + f) & loopbackMask_) * kChannels;
        out[f * kChannels + 0] = loopback_[idx + 0];
        out[f * kChannels + 1] = loopback_[idx + 1];
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
    params.SampleRate = 48000;   // Opus-native; nothing in the chain resamples
    params.ChannelCount = 2;
    params.Latency = 0;
    params.SafetyOffset = 0;
    params.EnableMixing = true;  // deliver the mixed system output to OnProcessMixedOutput
    params.CanBeDefault = true;
    params.CanBeDefaultForSystemSounds = true;

    auto device = std::make_shared<aspl::Device>(context, params);

    device->AddStreamWithControlsAsync(aspl::Direction::Output);
    device->AddStreamAsync(aspl::Direction::Input);

    ConfigureVolumeAndMute(device);

    device->SetIOHandler(handler);
    device->SetControlHandler(handler);

    return device;
}

} // namespace se_audio

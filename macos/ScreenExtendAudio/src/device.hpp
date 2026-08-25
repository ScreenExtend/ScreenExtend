// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).
//
// The ScreenExtend Audio virtual device (PRD-macos-legacy-audio.md §5, §8.1).
//
// It is an OUTPUT device so it can be set as the system default output; the system mixes every
// app's audio into it, and we capture that mix. It ALSO exposes an INPUT stream that mirrors the
// captured audio (a loopback), so the host has a HAL-input fallback when the shared-memory
// transport is unavailable (PRD §5.2a). Volume + Mute controls on the output scope keep the macOS
// volume keys alive (PRD §6.2, layer 1).

#pragma once

#include "shm_ring.hpp"

#include <aspl/ControlRequestHandler.hpp>
#include <aspl/IORequestHandler.hpp>

#include <atomic>
#include <memory>
#include <vector>

namespace aspl {
struct Context;
class Device;
} // namespace aspl

namespace se_audio {

// Realtime capture + loopback handler. Installed as both the device's IO handler and its control
// handler. Every method here runs on coreaudiod's realtime I/O thread: no allocation, no locks, no
// Obj-C, no syscalls (PRD §5.3, §10.3).
class CaptureHandler : public aspl::IORequestHandler, public aspl::ControlRequestHandler
{
public:
    explicit CaptureHandler(std::shared_ptr<ShmRing> shm);

    // Output path: the system mix for this cycle. We capture it BEFORE any volume/mute processing
    // (we deliberately do not call Stream::ApplyProcessing) so the streamed/tapped audio is always
    // full-scale and independent of the local volume — "mute locally, keep streaming" (PRD §6.2).
    void OnProcessMixedOutput(const std::shared_ptr<aspl::Stream>& stream,
        Float64 zeroTimestamp,
        Float64 timestamp,
        Float32* frames,
        UInt32 frameCount,
        UInt32 channelCount) override;

    // Input path (loopback / fallback transport): serve the previously-captured mix at the
    // requested timestamp. Fills silence where we have no data.
    void OnReadClientInput(const std::shared_ptr<aspl::Client>& client,
        const std::shared_ptr<aspl::Stream>& stream,
        Float64 zeroTimestamp,
        Float64 timestamp,
        void* bytes,
        UInt32 bytesCount) override;

private:
    std::shared_ptr<ShmRing> shm_;

    // Internal loopback ring indexed by absolute sample-time, feeding the input stream. Fixed
    // size, power-of-two, allocated once at construction. 2 channels interleaved.
    static constexpr uint32_t kLoopbackFrames = 65536; // 2^16 frames ≈ 1.37 s @ 48 kHz
    static constexpr uint32_t kChannels = 2;
    std::vector<float> loopback_; // kLoopbackFrames * kChannels, zero-initialised
    uint32_t loopbackMask_;       // kLoopbackFrames - 1
};

// Build the fully-configured ScreenExtend Audio device: output stream + volume + mute (§6.2), an
// input loopback stream, 48 kHz stereo, zero reported latency (§5.3). The returned device already
// has `handler` set as both its IO and control handler.
std::shared_ptr<aspl::Device> BuildDevice(const std::shared_ptr<aspl::Context>& context,
    const std::shared_ptr<CaptureHandler>& handler);

} // namespace se_audio

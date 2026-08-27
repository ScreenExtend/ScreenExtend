// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).

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

class CaptureHandler : public aspl::IORequestHandler, public aspl::ControlRequestHandler
{
public:
    explicit CaptureHandler(std::shared_ptr<ShmRing> shm);

    void OnProcessMixedOutput(const std::shared_ptr<aspl::Stream>& stream,
        Float64 zeroTimestamp,
        Float64 timestamp,
        Float32* frames,
        UInt32 frameCount,
        UInt32 channelCount) override;

    // loopback fallback: serve the captured mix at the requested timestamp, silence where we have none
    void OnReadClientInput(const std::shared_ptr<aspl::Client>& client,
        const std::shared_ptr<aspl::Stream>& stream,
        Float64 zeroTimestamp,
        Float64 timestamp,
        void* bytes,
        UInt32 bytesCount) override;

private:
    std::shared_ptr<ShmRing> shm_;

    // loopback ring indexed by absolute sample-time, power-of-two, allocated once at construction
    static constexpr uint32_t kLoopbackFrames = 65536; // 2^16 frames ≈ 1.37 s @ 48 kHz
    static constexpr uint32_t kChannels = 2;
    std::vector<float> loopback_;
    uint32_t loopbackMask_; // kLoopbackFrames - 1
};

std::shared_ptr<aspl::Device> BuildDevice(const std::shared_ptr<aspl::Context>& context,
    const std::shared_ptr<CaptureHandler>& handler);

} // namespace se_audio

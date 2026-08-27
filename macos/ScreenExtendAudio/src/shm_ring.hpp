// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).

#pragma once

#include <atomic>
#include <cstddef>
#include <cstdint>

namespace se_audio {

inline constexpr uint32_t kShmMagic = 0x31414553u;
inline constexpr uint32_t kLayoutVersion = 1u;

inline constexpr uint32_t kFormatF32Interleaved = 1u;

inline constexpr uint32_t kRingCapacity = 131072u; // 2^17
static_assert((kRingCapacity & (kRingCapacity - 1)) == 0, "capacity must be a power of two");

struct ShmHeader
{
    uint32_t magic;         // 0  : kShmMagic once initialised (published last on create)
    uint32_t version;       // 4  : kLayoutVersion
    uint32_t sampleRate;    // 8  : 48000
    uint32_t channels;      // 12 : 2
    uint32_t sampleFormat;  // 16 : kFormatF32Interleaved
    uint32_t capacity;      // 20 : kRingCapacity (samples)
    uint32_t reserved0;     // 24
    uint32_t reserved1;     // 28
    std::atomic<uint64_t> writePos;    // 32 : monotonic sample index (producer publishes, Release)
    std::atomic<uint64_t> generation;  // 40 : bumped each driver (re)start / format change
    std::atomic<uint64_t> overruns;    // 48 : informational; producer never blocks
    uint64_t reserved2;                // 56
    // ring: float[capacity] begins here (offset 64)
};

static_assert(sizeof(ShmHeader) == 64, "ShmHeader must be exactly 64 bytes (ABI with Rust reader)");
static_assert(offsetof(ShmHeader, writePos) == 32, "writePos offset is part of the ABI");
static_assert(offsetof(ShmHeader, generation) == 40, "generation offset is part of the ABI");

inline constexpr size_t kShmTotalBytes = sizeof(ShmHeader) + size_t(kRingCapacity) * sizeof(float);

class ShmRing
{
public:
    ShmRing() = default;
    ~ShmRing();

    ShmRing(const ShmRing&) = delete;
    ShmRing& operator=(const ShmRing&) = delete;

    bool Open();

    void Write(const float* frames, uint32_t sampleCount) noexcept;

    bool IsOpen() const noexcept { return header_ != nullptr; }

private:
    ShmHeader* header_ = nullptr;
    float* ring_ = nullptr;
    uint32_t mask_ = 0;
};

} // namespace se_audio

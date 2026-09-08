// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).

#pragma once

#include <atomic>
#include <cstddef>
#include <cstdint>

namespace se_audio {

inline constexpr uint32_t kShmMagic = 0x31414553u;

// Layout 2 grew the header from 64 to 128 bytes to carry the capture-time anchor and the reader's
// published position. A layout-1 segment left over from an older driver is re-initialised on open.
inline constexpr uint32_t kLayoutVersion = 2u;

inline constexpr uint32_t kFormatF32Interleaved = 1u;

inline constexpr uint32_t kRingCapacity = 131072u; // 2^17
static_assert((kRingCapacity & (kRingCapacity - 1)) == 0, "capacity must be a power of two");

inline constexpr uint32_t kChannelCount = 2u;
inline constexpr uint32_t kSampleRate = 48000u;

// ABI with src-tauri/src/macos_utils/audio/legacy/shm_reader.rs — change both together.
struct ShmHeader
{
    uint32_t magic;        // 0   : kShmMagic once initialised (published last on create)
    uint32_t version;      // 4   : kLayoutVersion
    uint32_t sampleRate;   // 8   : 48000
    uint32_t channels;     // 12  : 2
    uint32_t sampleFormat; // 16  : kFormatF32Interleaved
    uint32_t capacity;     // 20  : kRingCapacity (samples)
    uint32_t headerBytes;  // 24  : sizeof(ShmHeader)
    uint32_t reserved0;    // 28

    std::atomic<uint64_t> writePos;   // 32 : monotonic sample index (producer publishes, Release)
    std::atomic<uint64_t> generation; // 40 : bumped on each driver (re)start
    std::atomic<uint64_t> overruns;   // 48 : samples the producer overwrote unread
    std::atomic<uint64_t> readPos;    // 56 : consumer publishes (Release); 0 = no reader attached

    // Seqlock-guarded capture-time anchor: `anchorHostTime` is `mach_absolute_time()` at the
    // callback that wrote the block starting at sample index `anchorPos`. An odd `anchorSeq` means
    // a write is in flight. Without it a reader can only date samples from its own drain, which
    // folds the drain interval and every scheduling delay into the A/V sync offset.
    std::atomic<uint64_t> anchorSeq;      // 64
    std::atomic<uint64_t> anchorPos;      // 72
    std::atomic<uint64_t> anchorHostTime; // 80

    uint64_t reserved1[5]; // 88
    // ring: float[capacity] begins here (offset 128)
};

static_assert(
    sizeof(ShmHeader) == 128, "ShmHeader must be exactly 128 bytes (ABI with Rust reader)");
static_assert(offsetof(ShmHeader, magic) == 0, "magic offset is part of the ABI");
static_assert(offsetof(ShmHeader, version) == 4, "version offset is part of the ABI");
static_assert(offsetof(ShmHeader, sampleRate) == 8, "sampleRate offset is part of the ABI");
static_assert(offsetof(ShmHeader, channels) == 12, "channels offset is part of the ABI");
static_assert(offsetof(ShmHeader, sampleFormat) == 16, "sampleFormat offset is part of the ABI");
static_assert(offsetof(ShmHeader, capacity) == 20, "capacity offset is part of the ABI");
static_assert(offsetof(ShmHeader, headerBytes) == 24, "headerBytes offset is part of the ABI");
static_assert(offsetof(ShmHeader, writePos) == 32, "writePos offset is part of the ABI");
static_assert(offsetof(ShmHeader, generation) == 40, "generation offset is part of the ABI");
static_assert(offsetof(ShmHeader, overruns) == 48, "overruns offset is part of the ABI");
static_assert(offsetof(ShmHeader, readPos) == 56, "readPos offset is part of the ABI");
static_assert(offsetof(ShmHeader, anchorSeq) == 64, "anchorSeq offset is part of the ABI");
static_assert(offsetof(ShmHeader, anchorPos) == 72, "anchorPos offset is part of the ABI");
static_assert(
    offsetof(ShmHeader, anchorHostTime) == 80, "anchorHostTime offset is part of the ABI");

inline constexpr size_t kShmTotalBytes = sizeof(ShmHeader) + size_t(kRingCapacity) * sizeof(float);
static_assert(kShmTotalBytes == 128 + 131072 * 4, "total size is part of the ABI");

class ShmRing
{
public:
    ShmRing() = default;
    ~ShmRing();

    ShmRing(const ShmRing&) = delete;
    ShmRing& operator=(const ShmRing&) = delete;

    bool Open();

    /// Publish `sampleCount` interleaved-stereo floats captured at `hostTime` (mach ticks).
    /// Real-time safe: no allocation, no locks, never blocks.
    void Write(const float* frames, uint32_t sampleCount, uint64_t hostTime) noexcept;

    bool IsOpen() const noexcept { return header_ != nullptr; }

private:
    ShmHeader* header_ = nullptr;
    float* ring_ = nullptr;
    uint32_t mask_ = 0;
};

} // namespace se_audio

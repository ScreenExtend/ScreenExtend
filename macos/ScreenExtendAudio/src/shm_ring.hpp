// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).
//
// Shared-memory single-producer/single-consumer ring, WRITER side (PRD-macos-legacy-audio.md
// §5.2a, §5.3). The driver is the sole producer; it runs inside coreaudiod's real-time I/O thread
// and must never allocate, lock, syscall, or block there. It also must not care whether a reader
// exists — it always advances and overwrites the oldest data harmlessly.
//
// The byte layout below is a hard ABI contract with the Rust reader
// (`src-tauri/src/macos_utils/audio/legacy/shm_reader.rs`). Both sides are little-endian
// (x86_64 + arm64), 64-bit. Do not reorder fields or change sizes without bumping kLayoutVersion
// on BOTH sides.

#pragma once

#include <atomic>
#include <cstddef>
#include <cstdint>

namespace se_audio {

// 'SEA1' little-endian. Lets the reader validate it mapped the right, initialised segment.
inline constexpr uint32_t kShmMagic = 0x31414553u;
inline constexpr uint32_t kLayoutVersion = 1u;

// Sample format tag stored in the header (1 == 32-bit float, interleaved, native endian).
inline constexpr uint32_t kFormatF32Interleaved = 1u;

// Fixed ring capacity in f32 samples (power of two). 131072 samples = 64 KiB × 8 = 512 KiB of
// audio ≈ 1.37 s of 48 kHz stereo. Big enough to hide reader scheduling jitter; small enough to
// stay cache-friendly. The reader hard-codes the same constant (the segment is a fixed size).
inline constexpr uint32_t kRingCapacity = 131072u; // 2^17
static_assert((kRingCapacity & (kRingCapacity - 1)) == 0, "capacity must be a power of two");

// Header sits at offset 0; the f32 ring immediately follows at offset sizeof(ShmHeader).
// Laid out so the atomics land on 8-byte boundaries and the whole header is one cache line.
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

// Writer half of the shm ring. Created once on a non-realtime thread (driver construction); the
// realtime path only calls Write(), which is allocation/lock/syscall free.
class ShmRing
{
public:
    ShmRing() = default;
    ~ShmRing();

    ShmRing(const ShmRing&) = delete;
    ShmRing& operator=(const ShmRing&) = delete;

    // Create/attach the segment and initialise the header. NOT realtime-safe: call once at driver
    // construction, off the I/O thread. Returns false if shm_open/ftruncate/mmap fails (e.g. the
    // coreaudiod sandbox forbids POSIX shm — PRD §7.4 / §13.1); the driver then keeps working as a
    // pure HAL loopback and the host takes the hal_input fallback.
    bool Open();

    // Realtime-safe. Copy `sampleCount` interleaved f32 samples into the ring and publish. Never
    // blocks, never allocates, never touches a reader's state. Overwrites the oldest unread data
    // if the reader has fallen behind — the reader detects the lap and resyncs.
    void Write(const float* frames, uint32_t sampleCount) noexcept;

    bool IsOpen() const noexcept { return header_ != nullptr; }

private:
    ShmHeader* header_ = nullptr;
    float* ring_ = nullptr;
    uint32_t mask_ = 0;
};

} // namespace se_audio

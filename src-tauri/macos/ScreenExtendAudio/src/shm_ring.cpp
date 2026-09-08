// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).

#include "shm_ring.hpp"
#include "branding.hpp"
#include "log.hpp"

#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

namespace se_audio {

ShmRing::~ShmRing()
{
    if (header_ != nullptr) {
        ::munmap(static_cast<void*>(header_), kShmTotalBytes);
        header_ = nullptr;
        ring_ = nullptr;
    }
}

bool ShmRing::Open()
{
    // A POSIX shared-memory object on macOS can only be sized once: ftruncate on an already-sized
    // segment fails with EINVAL. A segment left over from an older driver is therefore not merely
    // stale, it is unusable — the layout-1 one is 64 bytes short. Unlinking first only removes the
    // name; a reader that still has it mapped keeps reading until it re-opens, and the generation
    // bump below is what tells it to.
    if (::shm_unlink(branding::kShmName) == 0) {
        SE_LOG("unlinked a pre-existing %{public}s segment", branding::kShmName);
    }

    // shm_open applies coreaudiod's umask to the mode, and this code does not control it: under
    // 077 the segment comes out 0600 and the host, running as the user, gets a bare EACCES from
    // its own shm_open. macOS has no fchmod for POSIX shared memory (it returns EINVAL) and the
    // object has no filesystem path to chmod, so clearing the umask across the create is the only
    // lever. The window is two calls at driver load.
    const mode_t prevMask = ::umask(0);
    int fd = ::shm_open(branding::kShmName, O_CREAT | O_EXCL | O_RDWR, 0666);
    if (fd < 0 && errno == EEXIST) {
        // another driver instance won the race; take what is there
        fd = ::shm_open(branding::kShmName, O_RDWR, 0666);
    }
    const int openErrno = errno;
    ::umask(prevMask);
    if (fd < 0) {
        SE_LOG_ERROR("shm_open(%{public}s) failed: %{public}s — the host will fall back to the "
                     "slower HAL-input transport",
            branding::kShmName,
            ::strerror(openErrno));
        return false;
    }

    // A POSIX shared-memory object can only be sized once on macOS, so EINVAL here means the
    // segment already had a size — fine if it is the size we want, fatal otherwise.
    struct stat st = {};
    if (::ftruncate(fd, static_cast<off_t>(kShmTotalBytes)) != 0
        && (::fstat(fd, &st) != 0 || size_t(st.st_size) != kShmTotalBytes)) {
        SE_LOG_ERROR("ftruncate(%{public}s, %zu) failed: %{public}s",
            branding::kShmName,
            kShmTotalBytes,
            ::strerror(errno));
        ::close(fd);
        return false;
    }
    if (::fstat(fd, &st) == 0 && (st.st_mode & 0666) != 0666) {
        SE_LOG_ERROR("%{public}s is mode 0%{public}o, not 0666 — the host may not be able to map "
                     "it and will fall back to the HAL-input transport",
            branding::kShmName,
            unsigned(st.st_mode & 0777));
    }

    void* base = ::mmap(nullptr, kShmTotalBytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    ::close(fd); // mapping keeps the segment alive
    if (base == MAP_FAILED) {
        SE_LOG_ERROR("mmap(%{public}s) failed: %{public}s", branding::kShmName, ::strerror(errno));
        return false;
    }

    header_ = static_cast<ShmHeader*>(base);
    ring_ = reinterpret_cast<float*>(static_cast<char*>(base) + sizeof(ShmHeader));
    mask_ = kRingCapacity - 1;

    // A stale segment from an older driver keeps the magic but carries a different layout, so the
    // version and the geometry decide, not the magic alone.
    const bool stale = header_->magic != kShmMagic || header_->version != kLayoutVersion
        || header_->headerBytes != sizeof(ShmHeader) || header_->capacity != kRingCapacity;
    if (stale) {
        SE_LOG("initialising shared segment %{public}s (was magic=0x%{public}x version=%{public}u)",
            branding::kShmName,
            header_->magic,
            header_->version);
        header_->magic = 0; // no reader may trust the header while it is being rewritten
        std::atomic_thread_fence(std::memory_order_release);
        header_->version = kLayoutVersion;
        header_->sampleRate = kSampleRate;
        header_->channels = kChannelCount;
        header_->sampleFormat = kFormatF32Interleaved;
        header_->capacity = kRingCapacity;
        header_->headerBytes = static_cast<uint32_t>(sizeof(ShmHeader));
        header_->reserved0 = 0;
        for (auto& r : header_->reserved1) {
            r = 0;
        }
        header_->writePos.store(0, std::memory_order_relaxed);
        header_->overruns.store(0, std::memory_order_relaxed);
        header_->readPos.store(0, std::memory_order_relaxed);
        header_->anchorSeq.store(0, std::memory_order_relaxed);
        header_->anchorPos.store(0, std::memory_order_relaxed);
        header_->anchorHostTime.store(0, std::memory_order_relaxed);
        header_->generation.store(0, std::memory_order_relaxed);
        std::atomic_thread_fence(std::memory_order_release);
        header_->magic = kShmMagic;
    }

    const uint64_t generation = header_->generation.fetch_add(1, std::memory_order_release) + 1;
    SE_LOG("shared segment %{public}s open: %{public}u Hz x%{public}u, %{public}u samples, "
           "generation %{public}llu",
        branding::kShmName,
        kSampleRate,
        kChannelCount,
        kRingCapacity,
        static_cast<unsigned long long>(generation));
    return true;
}

void ShmRing::Write(const float* frames, uint32_t sampleCount, uint64_t hostTime) noexcept
{
    if (header_ == nullptr || frames == nullptr || sampleCount == 0) {
        return;
    }
    // A block bigger than the whole ring cannot be published; keep the newest ring-full and count
    // the rest as lost rather than running the wrap arithmetic off the end of the mapping.
    if (sampleCount > kRingCapacity) {
        const uint32_t skipped = sampleCount - kRingCapacity;
        header_->overruns.fetch_add(skipped, std::memory_order_relaxed);
        frames += skipped;
        sampleCount = kRingCapacity;
    }
    // single producer: relaxed load of our own writePos is fine
    const uint64_t w = header_->writePos.load(std::memory_order_relaxed);

    // Account for what this write overwrites unread. readPos == 0 means no reader has attached
    // yet — the ring is simply free-running and nothing is being lost.
    const uint64_t r = header_->readPos.load(std::memory_order_acquire);
    if (r != 0) {
        const uint64_t backlog = w - r;
        const uint64_t after = backlog + sampleCount;
        if (after > kRingCapacity) {
            header_->overruns.fetch_add(after - kRingCapacity, std::memory_order_relaxed);
        }
    }

    // Publish the capture time of this block before the samples it describes, so a reader that
    // acquires writePos and then reads the anchor can only ever see an anchor at or behind it.
    const uint64_t seq = header_->anchorSeq.load(std::memory_order_relaxed);
    header_->anchorSeq.store(seq + 1, std::memory_order_relaxed); // odd: write in flight
    std::atomic_thread_fence(std::memory_order_release);
    header_->anchorPos.store(w, std::memory_order_relaxed);
    header_->anchorHostTime.store(hostTime, std::memory_order_relaxed);
    header_->anchorSeq.store(seq + 2, std::memory_order_release);

    // two memcpys around the wrap rather than a scalar loop: this is a real-time callback
    const uint32_t start = static_cast<uint32_t>(w & mask_);
    const uint32_t first = (start + sampleCount > kRingCapacity) ? (kRingCapacity - start)
                                                                 : sampleCount;
    ::memcpy(ring_ + start, frames, size_t(first) * sizeof(float));
    if (first < sampleCount) {
        ::memcpy(ring_, frames + first, size_t(sampleCount - first) * sizeof(float));
    }

    // release: sample stores happen-before any reader that acquire-loads writePos
    header_->writePos.store(w + sampleCount, std::memory_order_release);
}

} // namespace se_audio

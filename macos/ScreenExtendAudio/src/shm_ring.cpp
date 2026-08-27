// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).

#include "shm_ring.hpp"
#include "branding.hpp"

#include <fcntl.h>
#include <sys/mman.h>
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
    const int fd = ::shm_open(branding::kShmName, O_CREAT | O_RDWR, 0666);
    if (fd < 0) {
        return false; // sandbox denied or wrong perms — host falls back to HAL
    }

    // ftruncate on an already-sized segment is harmless
    if (::ftruncate(fd, static_cast<off_t>(kShmTotalBytes)) != 0) {
        ::close(fd);
        return false;
    }

    void* base = ::mmap(nullptr,
        kShmTotalBytes,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd,
        0);
    ::close(fd); // mapping keeps the segment alive
    if (base == MAP_FAILED) {
        return false;
    }

    header_ = static_cast<ShmHeader*>(base);
    ring_ = reinterpret_cast<float*>(static_cast<char*>(base) + sizeof(ShmHeader));
    mask_ = kRingCapacity - 1;

    if (header_->magic != kShmMagic) {
        header_->version = kLayoutVersion;
        header_->sampleRate = 48000;
        header_->channels = 2;
        header_->sampleFormat = kFormatF32Interleaved;
        header_->capacity = kRingCapacity;
        header_->reserved0 = 0;
        header_->reserved1 = 0;
        header_->reserved2 = 0;
        header_->writePos.store(0, std::memory_order_relaxed);
        header_->overruns.store(0, std::memory_order_relaxed);
        header_->generation.store(0, std::memory_order_relaxed);
        std::atomic_thread_fence(std::memory_order_release);
        header_->magic = kShmMagic;
    }

    header_->generation.fetch_add(1, std::memory_order_release);
    return true;
}

void ShmRing::Write(const float* frames, uint32_t sampleCount) noexcept
{
    if (header_ == nullptr || frames == nullptr || sampleCount == 0) {
        return;
    }
    // single producer: relaxed load of our own writePos is fine
    const uint64_t w = header_->writePos.load(std::memory_order_relaxed);
    for (uint32_t i = 0; i < sampleCount; ++i) {
        ring_[(w + i) & mask_] = frames[i];
    }
    // release: sample stores happen-before any reader that acquire-loads writePos
    header_->writePos.store(w + sampleCount, std::memory_order_release);
}

} // namespace se_audio

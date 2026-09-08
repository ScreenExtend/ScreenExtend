// Usage:
//   clang tools/shmcheck.c -o /tmp/shmcheck
//   /tmp/shmcheck            # one snapshot
//   /tmp/shmcheck watch      # poll for a few seconds, print writePos delta + RMS
//
// Reads the layout-2 header defined in src/shm_ring.hpp. Read-only, so it never disturbs the
// host's own readPos.

#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define CAP 131072u
#define HDR 128u
#define TOTAL (HDR + CAP * 4u)

#define OFF_WRITE_POS 32
#define OFF_GENERATION 40
#define OFF_OVERRUNS 48
#define OFF_READ_POS 56
#define OFF_ANCHOR_SEQ 64
#define OFF_ANCHOR_POS 72
#define OFF_ANCHOR_HOST 80

static double rms_recent(const float* ring, uint64_t writePos, uint32_t n) {
    if (n > CAP) n = CAP;
    if ((uint64_t)n > writePos) n = (uint32_t)writePos;
    double sum = 0;
    for (uint32_t i = 0; i < n; i++) {
        uint64_t idx = (writePos - 1 - i) & (CAP - 1);
        double s = ring[idx];
        sum += s * s;
    }
    return n ? sqrt(sum / n) : 0.0;
}

static volatile uint64_t* u64_at(void* p, unsigned off) {
    return (volatile uint64_t*)((char*)p + off);
}

int main(int argc, char** argv) {
    // The host opens O_RDWR so it can publish readPos; report whether that is granted, since a
    // segment created under a restrictive coreaudiod umask is the difference between the fast
    // shared-memory transport and the HAL-input fallback.
    int rw = shm_open("/ScreenExtendAudio", O_RDWR, 0);
    printf("open O_RDWR (what the host needs): %s\n", rw >= 0 ? "granted" : strerror(errno));
    if (rw >= 0) close(rw);

    int fd = shm_open("/ScreenExtendAudio", O_RDONLY, 0);
    if (fd < 0) { perror("shm_open"); return 1; }
    void* p = mmap(0, TOTAL, PROT_READ, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) { perror("mmap"); return 2; }
    uint32_t* h = (uint32_t*)p;
    volatile uint64_t* wp = u64_at(p, OFF_WRITE_POS);
    volatile uint64_t* gen = u64_at(p, OFF_GENERATION);
    volatile uint64_t* over = u64_at(p, OFF_OVERRUNS);
    volatile uint64_t* rp = u64_at(p, OFF_READ_POS);
    volatile uint64_t* anchorPos = u64_at(p, OFF_ANCHOR_POS);
    volatile uint64_t* anchorHost = u64_at(p, OFF_ANCHOR_HOST);
    const float* ring = (const float*)((char*)p + HDR);

    printf("header magic=0x%08x version=%u rate=%u ch=%u fmt=%u cap=%u hdr=%u\n",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6]);
    if (h[1] != 2u || h[6] != HDR) {
        printf("warning: this tool reads layout 2 with a %u-byte header; the segment says "
               "version=%u hdr=%u\n", HDR, h[1], h[6]);
    }

    if (argc > 1 && strcmp(argv[1], "watch") == 0) {
        uint64_t prev = *wp;
        for (int i = 0; i < 10; i++) {
            usleep(300000);
            uint64_t now = *wp;
            double rms = rms_recent(ring, now, 4800);
            printf("t=%0.1fs writePos=%llu (+%llu) readPos=%llu backlog=%llu gen=%llu "
                   "overruns=%llu rms=%.5f %s\n",
                (i + 1) * 0.3, now, now - prev, *rp, *rp ? now - *rp : 0, *gen, *over, rms,
                rms > 1e-4 ? "NON-SILENT" : "silent");
            prev = now;
        }
    } else {
        printf("writePos=%llu readPos=%llu generation=%llu overruns=%llu rms=%.5f\n",
            *wp, *rp, *gen, *over, rms_recent(ring, *wp, 4800));
        printf("anchor: pos=%llu hostTime=%llu (mach ticks)\n", *anchorPos, *anchorHost);
    }
    return 0;
}

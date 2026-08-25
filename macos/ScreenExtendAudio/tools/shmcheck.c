// Reads the ScreenExtend Audio shm capture ring (/ScreenExtendAudio) and reports the header plus a
// running RMS over the most recent samples — proves the driver is writing captured audio. Dev tool.
//
//   clang tools/shmcheck.c -o /tmp/shmcheck
//   /tmp/shmcheck            # one snapshot
//   /tmp/shmcheck watch      # poll for a few seconds, print writePos delta + RMS

#include <fcntl.h>
#include <sys/mman.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <math.h>
#include <unistd.h>

#define CAP 131072u
#define HDR 64u
#define TOTAL (HDR + CAP * 4u)

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

int main(int argc, char** argv) {
    int fd = shm_open("/ScreenExtendAudio", O_RDONLY, 0);
    if (fd < 0) { perror("shm_open"); return 1; }
    void* p = mmap(0, TOTAL, PROT_READ, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) { perror("mmap"); return 2; }
    uint32_t* h = (uint32_t*)p;
    volatile uint64_t* wp = (uint64_t*)((char*)p + 32);
    volatile uint64_t* gen = (uint64_t*)((char*)p + 40);
    const float* ring = (const float*)((char*)p + HDR);

    printf("header magic=0x%08x version=%u rate=%u ch=%u cap=%u\n", h[0], h[1], h[2], h[3], h[5]);

    if (argc > 1 && strcmp(argv[1], "watch") == 0) {
        uint64_t prev = *wp;
        for (int i = 0; i < 10; i++) {
            usleep(300000);
            uint64_t now = *wp;
            double rms = rms_recent(ring, now, 4800);
            printf("t=%0.1fs writePos=%llu (+%llu) gen=%llu rms=%.5f %s\n",
                (i + 1) * 0.3, now, now - prev, *gen, rms, rms > 1e-4 ? "NON-SILENT" : "silent");
            prev = now;
        }
    } else {
        printf("writePos=%llu generation=%llu rms=%.5f\n", *wp, *gen, rms_recent(ring, *wp, 4800));
    }
    return 0;
}

/*
 * cursor-mover: motion source for the macOS real-session e2e test (run-mac.mjs).
 *
 *   cursor-mover list                 JSON array of the online displays
 *   cursor-mover move <id> <seconds>  warp the cursor around display <id>'s bounds, then print
 *                                     a one-line JSON summary
 *
 * CGWarpMouseCursorPosition moves the cursor without posting an event, so it needs no
 * Accessibility grant. The cursor is composited into the capture (CGDisplayStream
 * kCGDisplayStreamShowCursor / SCK showsCursor), so a moving cursor is a changing frame.
 *
 * Build (universal, 10.15 floor):
 *   clang -O2 -arch arm64 -arch x86_64 -mmacosx-version-min=10.15 \
 *     -framework CoreGraphics -framework CoreFoundation -o cursor-mover cursor-mover.c
 */
#include <CoreGraphics/CoreGraphics.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static double now_s(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static int list(void) {
    CGDirectDisplayID ids[32];
    uint32_t n = 0;
    if (CGGetOnlineDisplayList(32, ids, &n) != kCGErrorSuccess) {
        fprintf(stderr, "CGGetOnlineDisplayList failed (no WindowServer connection?)\n");
        return 2;
    }
    printf("[");
    for (uint32_t i = 0; i < n; i++) {
        CGRect b = CGDisplayBounds(ids[i]);
        printf("%s{\"id\":%u,\"x\":%d,\"y\":%d,\"w\":%d,\"h\":%d,\"main\":%s,\"builtin\":%s,"
               "\"active\":%s,\"vendor\":%u,\"model\":%u,\"serial\":%u,\"pixelsWide\":%zu,"
               "\"pixelsHigh\":%zu}",
               i ? "," : "", ids[i], (int)b.origin.x, (int)b.origin.y, (int)b.size.width,
               (int)b.size.height, CGDisplayIsMain(ids[i]) ? "true" : "false",
               CGDisplayIsBuiltin(ids[i]) ? "true" : "false",
               CGDisplayIsActive(ids[i]) ? "true" : "false", CGDisplayVendorNumber(ids[i]),
               CGDisplayModelNumber(ids[i]), CGDisplaySerialNumber(ids[i]),
               CGDisplayPixelsWide(ids[i]), CGDisplayPixelsHigh(ids[i]));
    }
    printf("]\n");
    return 0;
}

static int move(CGDirectDisplayID id, double seconds) {
    CGRect b = CGDisplayBounds(id);
    if (b.size.width < 4 || b.size.height < 4) {
        printf("{\"ok\":false,\"error\":\"display %u has no bounds\"}\n", id);
        return 2;
    }
    CGEventRef e = CGEventCreate(NULL);
    CGPoint home = e ? CGEventGetLocation(e) : CGPointZero;
    if (e) CFRelease(e);
    const double margin = 8.0;
    const double w = b.size.width - 2 * margin, h = b.size.height - 2 * margin;
    long warps = 0, errors = 0;
    double t0 = now_s(), t;
    while ((t = now_s() - t0) < seconds) {
        /* Lissajous 3:2 over the whole display, ~1 sweep per 1.5 s: every frame changes. */
        CGPoint p = CGPointMake(b.origin.x + margin + w * (0.5 + 0.5 * sin(t * 4.2)),
                                b.origin.y + margin + h * (0.5 + 0.5 * sin(t * 2.8 + 0.7)));
        if (CGWarpMouseCursorPosition(p) == kCGErrorSuccess) warps++;
        else errors++;
        usleep(4000); /* ~250 Hz, above any refresh rate the host offers */
    }
    CGWarpMouseCursorPosition(home);
    printf("{\"ok\":%s,\"display\":%u,\"bounds\":[%d,%d,%d,%d],\"seconds\":%.2f,"
           "\"warps\":%ld,\"errors\":%ld}\n",
           warps > 0 ? "true" : "false", id, (int)b.origin.x, (int)b.origin.y,
           (int)b.size.width, (int)b.size.height, now_s() - t0, warps, errors);
    return warps > 0 ? 0 : 3;
}

int main(int argc, char **argv) {
    /* Connect to the WindowServer through the display list before anything else. On 10.15 over
     * SSH, a first call of CGDisplayBounds deadlocks inside SkyLight's one-time init (sampled:
     * SLSInitialize -> initDisplayState -> os_nospin_lock), while the display-list path is fine. */
    {
        CGDirectDisplayID ids[32];
        uint32_t n = 0;
        CGGetOnlineDisplayList(32, ids, &n);
    }
    if (argc >= 2 && !strcmp(argv[1], "list")) return list();
    if (argc >= 4 && !strcmp(argv[1], "move"))
        return move((CGDirectDisplayID)strtoul(argv[2], NULL, 10), atof(argv[3]));
    fprintf(stderr, "usage: %s list | move <display-id> <seconds>\n", argv[0]);
    return 64;
}

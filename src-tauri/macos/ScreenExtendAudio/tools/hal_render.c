// Play a tone into the ScreenExtend Audio device, so the capture path can be exercised without a
// GUI session. `say` and `afplay` both go through AudioQueue, which refuses to start over SSH
// (-66681), and the driver only calls OnProcessMixedOutput while something is rendering to it.
// This is a plain HAL IOProc, the same API the Rust side uses, and it works headless.
//
// Usage:
//   clang tools/hal_render.c -o /tmp/hal_render -framework CoreAudio -framework CoreFoundation
//   /tmp/hal_render               # 5 seconds of 440 Hz into the ScreenExtend device
//   /tmp/hal_render 10 220        # 10 seconds of 220 Hz
//   /tmp/hal_render 5 440 <uid>   # into some other device, to sanity-check the environment

#include <CoreAudio/CoreAudio.h>
#include <CoreFoundation/CoreFoundation.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static const char* DEVICE_UID = "app.screenextend.desktop.audio.device";

static double g_phase = 0.0;
static double g_step = 0.0;
static volatile unsigned long g_frames = 0;
static volatile unsigned long g_cycles = 0;

static AudioObjectID find_device_by_uid(const char* target) {
    AudioObjectPropertyAddress a = {kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMaster};
    UInt32 size = 0;
    if (AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &a, 0, 0, &size)) return 0;
    int n = size / sizeof(AudioObjectID);
    AudioObjectID* ids = malloc(size);
    if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, 0, 0, &size, ids)) {
        free(ids);
        return 0;
    }
    AudioObjectID found = 0;
    for (int i = 0; i < n; i++) {
        CFStringRef uid = NULL;
        UInt32 sz = sizeof(uid);
        AudioObjectPropertyAddress ua = {kAudioDevicePropertyDeviceUID,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMaster};
        if (AudioObjectGetPropertyData(ids[i], &ua, 0, 0, &sz, &uid) == 0 && uid) {
            char buf[256];
            CFStringGetCString(uid, buf, sizeof(buf), kCFStringEncodingUTF8);
            CFRelease(uid);
            if (strcmp(buf, target) == 0) { found = ids[i]; break; }
        }
    }
    free(ids);
    return found;
}

static OSStatus render(AudioObjectID dev, const AudioTimeStamp* now,
    const AudioBufferList* in, const AudioTimeStamp* inTime, AudioBufferList* out,
    const AudioTimeStamp* outTime, void* ctx) {
    (void)dev; (void)now; (void)in; (void)inTime; (void)outTime; (void)ctx;
    if (!out) return noErr;
    g_cycles++;
    for (UInt32 b = 0; b < out->mNumberBuffers; b++) {
        float* p = (float*)out->mBuffers[b].mData;
        if (!p) continue;
        UInt32 ch = out->mBuffers[b].mNumberChannels;
        if (ch == 0) continue;
        UInt32 frames = out->mBuffers[b].mDataByteSize / (ch * sizeof(float));
        for (UInt32 f = 0; f < frames; f++) {
            float s = (float)(0.25 * sin(g_phase));
            g_phase += g_step;
            if (g_phase > 2.0 * M_PI) g_phase -= 2.0 * M_PI;
            for (UInt32 c = 0; c < ch; c++) p[f * ch + c] = s;
        }
        g_frames += frames;
    }
    return noErr;
}

int main(int argc, char** argv) {
    double seconds = argc > 1 ? atof(argv[1]) : 5.0;
    double hz = argc > 2 ? atof(argv[2]) : 440.0;
    const char* uid = argc > 3 ? argv[3] : DEVICE_UID;

    AudioObjectID dev = find_device_by_uid(uid);
    if (!dev) { fprintf(stderr, "device %s not found\n", uid); return 1; }

    Float64 rate = 48000.0;
    UInt32 sz = sizeof(rate);
    AudioObjectPropertyAddress ra = {kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMaster};
    AudioObjectGetPropertyData(dev, &ra, 0, 0, &sz, &rate);
    g_step = 2.0 * M_PI * hz / rate;

    AudioDeviceIOProcID proc = NULL;
    OSStatus st = AudioDeviceCreateIOProcID(dev, render, NULL, &proc);
    if (st) { fprintf(stderr, "AudioDeviceCreateIOProcID failed: %d\n", (int)st); return 2; }
    st = AudioDeviceStart(dev, proc);
    if (st) { fprintf(stderr, "AudioDeviceStart failed: %d\n", (int)st); return 3; }

    printf("rendering %.1f Hz for %.1f s into %s (rate %.0f)\n", hz, seconds, uid, rate);
    fflush(stdout);
    usleep((useconds_t)(seconds * 1e6));

    AudioDeviceStop(dev, proc);
    AudioDeviceDestroyIOProcID(dev, proc);
    printf("done: %lu io cycles, %lu frames rendered\n", g_cycles, g_frames);
    return 0;
}

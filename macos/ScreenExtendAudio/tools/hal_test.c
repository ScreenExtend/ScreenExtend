// Diagnostic harness for the ScreenExtend Audio virtual device (macOS legacy tier).
// Not shipped — a developer tool for verifying the loaded driver on 10.15–12.x.
//
//   clang tools/hal_test.c -o /tmp/hal_test -framework CoreAudio -framework CoreFoundation
//   /tmp/hal_test              # print device info (id, buffer range, volume/mute controls)
//   /tmp/hal_test setdefault   # + make it the system default output
//   /tmp/hal_test setvol 0.5   # + set its output volume scalar
//   /tmp/hal_test setmute 1    # + set its output mute
//
// Pairs with the shm reader (/ScreenExtendAudio) to prove the end-to-end capture path.

#include <CoreAudio/CoreAudio.h>
#include <CoreFoundation/CoreFoundation.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

static const char* DEVICE_UID = "app.screenextend.desktop.audio.device";

static AudioObjectID find_device_by_uid(const char* target) {
    AudioObjectPropertyAddress a = {kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMaster};
    UInt32 size = 0;
    if (AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &a, 0, 0, &size)) return 0;
    int n = size / sizeof(AudioObjectID);
    AudioObjectID* ids = malloc(size);
    if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, 0, 0, &size, ids)) { free(ids); return 0; }
    AudioObjectID found = 0;
    for (int i = 0; i < n; i++) {
        CFStringRef uid = NULL; UInt32 sz = sizeof(uid);
        AudioObjectPropertyAddress ua = {kAudioDevicePropertyDeviceUID,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMaster};
        if (AudioObjectGetPropertyData(ids[i], &ua, 0, 0, &sz, &uid) == 0 && uid) {
            char buf[256]; CFStringGetCString(uid, buf, sizeof(buf), kCFStringEncodingUTF8);
            CFRelease(uid);
            if (strcmp(buf, target) == 0) { found = ids[i]; break; }
        }
    }
    free(ids);
    return found;
}

static void print_default_output_uid(void) {
    AudioObjectPropertyAddress da = {kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMaster};
    AudioObjectID d = 0; UInt32 sz = sizeof(d);
    if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &da, 0, 0, &sz, &d)) return;
    AudioObjectPropertyAddress ua = {kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMaster};
    CFStringRef uid = NULL; sz = sizeof(uid);
    if (AudioObjectGetPropertyData(d, &ua, 0, 0, &sz, &uid) == 0 && uid) {
        char buf[256]; CFStringGetCString(uid, buf, sizeof(buf), kCFStringEncodingUTF8);
        CFRelease(uid);
        printf("RESULT default_output_uid=%s id=%u\n", buf, d);
    }
}

int main(int argc, char** argv) {
    if (argc > 1 && strcmp(argv[1], "defaultuid") == 0) { print_default_output_uid(); return 0; }
    AudioObjectID dev = find_device_by_uid(DEVICE_UID);
    if (!dev) { printf("RESULT device_found=0\n"); return 1; }
    printf("RESULT device_found=1 id=%u\n", dev);

    AudioObjectPropertyAddress ra = {'fsz#', kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMaster};
    AudioValueRange r; UInt32 sz = sizeof(r);
    if (AudioObjectGetPropertyData(dev, &ra, 0, 0, &sz, &r) == 0)
        printf("RESULT buffer_range=[%.0f,%.0f]\n", r.mMinimum, r.mMaximum);

    AudioObjectPropertyAddress ba = {'fsiz', kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMaster};
    UInt32 bfs; sz = sizeof(bfs);
    if (AudioObjectGetPropertyData(dev, &ba, 0, 0, &sz, &bfs) == 0)
        printf("RESULT buffer_frame_size=%u period_ms=%.2f\n", bfs, bfs / 48.0);

    AudioObjectPropertyAddress va = {kAudioDevicePropertyVolumeScalar,
        kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMaster};
    Float32 vol = -1; sz = sizeof(vol);
    OSStatus vst = AudioObjectGetPropertyData(dev, &va, 0, 0, &sz, &vol);
    printf("RESULT volume_control_present=%d st=%d val=%.3f\n", vst == 0, (int)vst, vol);

    AudioObjectPropertyAddress ma = {kAudioDevicePropertyMute,
        kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMaster};
    UInt32 mute = 0; sz = sizeof(mute);
    OSStatus mst = AudioObjectGetPropertyData(dev, &ma, 0, 0, &sz, &mute);
    printf("RESULT mute_control_present=%d st=%d val=%u\n", mst == 0, (int)mst, mute);

    if (argc > 1 && strcmp(argv[1], "setdefault") == 0) {
        AudioObjectPropertyAddress da = {kAudioHardwarePropertyDefaultOutputDevice,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMaster};
        OSStatus st = AudioObjectSetPropertyData(kAudioObjectSystemObject, &da, 0, 0, sizeof(dev), &dev);
        printf("RESULT set_default_output st=%d\n", (int)st);
    }
    if (argc > 2 && strcmp(argv[1], "setvol") == 0) {
        Float32 v = atof(argv[2]);
        OSStatus st = AudioObjectSetPropertyData(dev, &va, 0, 0, sizeof(v), &v);
        printf("RESULT set_volume st=%d val=%.3f\n", (int)st, v);
    }
    if (argc > 2 && strcmp(argv[1], "setmute") == 0) {
        UInt32 m = atoi(argv[2]);
        OSStatus st = AudioObjectSetPropertyData(dev, &ma, 0, 0, sizeof(m), &m);
        printf("RESULT set_mute st=%d val=%u\n", (int)st, m);
    }
    if (argc > 2 && strcmp(argv[1], "setbuffer") == 0) {
        UInt32 n = atoi(argv[2]);
        OSStatus st = AudioObjectSetPropertyData(dev, &ba, 0, 0, sizeof(n), &n);
        UInt32 got = 0; UInt32 gs = sizeof(got);
        AudioObjectGetPropertyData(dev, &ba, 0, 0, &gs, &got);
        printf("RESULT set_buffer st=%d requested=%u got=%u period_ms=%.2f\n", (int)st, n, got, got / 48.0);
    }
    if (argc > 1 && strcmp(argv[1], "restore") == 0) {
        // Set default output to the first output device that isn't us (restores real audio).
        AudioObjectPropertyAddress la = {kAudioHardwarePropertyDevices,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMaster};
        UInt32 size = 0; AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &la, 0, 0, &size);
        int n = size / sizeof(AudioObjectID); AudioObjectID* ids = malloc(size);
        AudioObjectGetPropertyData(kAudioObjectSystemObject, &la, 0, 0, &size, ids);
        for (int i = 0; i < n; i++) {
            if (ids[i] == dev) continue;
            AudioObjectPropertyAddress sa = {kAudioDevicePropertyStreamConfiguration,
                kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMaster};
            UInt32 csz = 0;
            if (AudioObjectGetPropertyDataSize(ids[i], &sa, 0, 0, &csz) || csz < sizeof(AudioBufferList)) continue;
            AudioBufferList* bl = malloc(csz);
            AudioObjectGetPropertyData(ids[i], &sa, 0, 0, &csz, bl);
            UInt32 ch = 0; for (UInt32 b = 0; b < bl->mNumberBuffers; b++) ch += bl->mBuffers[b].mNumberChannels;
            free(bl);
            if (ch == 0) continue;
            AudioObjectPropertyAddress da = {kAudioHardwarePropertyDefaultOutputDevice,
                kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMaster};
            OSStatus st = AudioObjectSetPropertyData(kAudioObjectSystemObject, &da, 0, 0, sizeof(ids[i]), &ids[i]);
            printf("RESULT restore_default_to=%u st=%d\n", ids[i], (int)st);
            break;
        }
        free(ids);
    }
    return 0;
}

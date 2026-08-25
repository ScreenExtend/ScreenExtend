// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).
//
// AudioServerPlugIn entry point for the ScreenExtend Audio virtual device
// (PRD-macos-legacy-audio.md §8.1). coreaudiod loads this bundle from
// /Library/Audio/Plug-Ins/HAL/ScreenExtendAudio.driver, reads Info.plist, and calls the factory
// named there (must equal branding::kFactoryUUID). The factory constructs the whole libASPL object
// hierarchy once and returns the driver's HAL reference.

#include "branding.hpp"
#include "device.hpp"
#include "shm_ring.hpp"

#include <aspl/Context.hpp>
#include <aspl/Driver.hpp>
#include <aspl/Plugin.hpp>

#include <CoreAudio/AudioServerPlugIn.h>

#include <memory>

namespace {

std::shared_ptr<aspl::Driver> CreateDriver()
{
    auto context = std::make_shared<aspl::Context>();

    // Shared-memory writer. Created here, off the realtime path (PRD §5.2a). If shm is unavailable
    // (coreaudiod sandbox denies POSIX shm — PRD §7.4), Open() returns false and the device keeps
    // working as a pure HAL loopback; the host then reads via its input stream instead. We keep the
    // ShmRing alive regardless via the handler.
    auto shm = std::make_shared<se_audio::ShmRing>();
    shm->Open(); // best-effort; a failed open is not fatal.

    auto handler = std::make_shared<se_audio::CaptureHandler>(shm);
    auto device = se_audio::BuildDevice(context, handler);

    auto plugin = std::make_shared<aspl::Plugin>(context);
    plugin->AddDevice(device);

    return std::make_shared<aspl::Driver>(context, plugin);
}

} // namespace

// Exported explicitly: the whole plug-in is built with -fvisibility=hidden, but coreaudiod resolves
// this factory by name (CFBundleGetFunctionPointerForName) via the CFPlugInFactories entry in
// Info.plist, so it must remain a visible dynamic symbol.
extern "C" __attribute__((visibility("default"))) void* ScreenExtendAudioEntryPoint(
    CFAllocatorRef /*allocator*/, CFUUIDRef typeUUID)
{
    // Only respond to the AudioServerPlugIn type UUID.
    if (!CFEqual(typeUUID, kAudioServerPlugInTypeUUID)) {
        return nullptr;
    }

    // Constructed once and kept alive for the lifetime of coreaudiod's mapping of this bundle.
    static std::shared_ptr<aspl::Driver> driver = CreateDriver();

    return driver->GetReference();
}

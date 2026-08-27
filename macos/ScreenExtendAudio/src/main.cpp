// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).

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

    auto shm = std::make_shared<se_audio::ShmRing>();
    shm->Open(); // best-effort; a failed open is not fatal

    auto handler = std::make_shared<se_audio::CaptureHandler>(shm);
    auto device = se_audio::BuildDevice(context, handler);

    auto plugin = std::make_shared<aspl::Plugin>(context);
    plugin->AddDevice(device);

    return std::make_shared<aspl::Driver>(context, plugin);
}

} // namespace

extern "C" __attribute__((visibility("default"))) void* ScreenExtendAudioEntryPoint(
    CFAllocatorRef /*allocator*/, CFUUIDRef typeUUID)
{
    if (!CFEqual(typeUUID, kAudioServerPlugInTypeUUID)) {
        return nullptr;
    }

    static std::shared_ptr<aspl::Driver> driver = CreateDriver();

    return driver->GetReference();
}

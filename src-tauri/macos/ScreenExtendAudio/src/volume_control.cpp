// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).

#include "volume_control.hpp"

#include <aspl/Device.hpp>
#include <aspl/MuteControl.hpp>
#include <aspl/VolumeControl.hpp>

#include <CoreAudio/AudioServerPlugIn.h>

namespace se_audio {

void ConfigureVolumeAndMute(const std::shared_ptr<aspl::Device>& device)
{
    if (auto volume = device->GetVolumeControlByIndex(kAudioObjectPropertyScopeOutput, 0)) {
        volume->SetScalarValue(1.0f);
    }

    if (auto mute = device->GetMuteControlByIndex(kAudioObjectPropertyScopeOutput, 0)) {
        mute->SetIsMuted(false);
    }
}

} // namespace se_audio

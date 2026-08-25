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
    // Output-scope volume: start at unity (1.0). macOS reads this back as the current output
    // level; the user's key presses drive it from here, and the host mirrors it to the real
    // device + applies it to the monitor path.
    if (auto volume = device->GetVolumeControlByIndex(kAudioObjectPropertyScopeOutput, 0)) {
        volume->SetScalarValue(1.0f);
    }

    // Output-scope mute: start unmuted.
    if (auto mute = device->GetMuteControlByIndex(kAudioObjectPropertyScopeOutput, 0)) {
        mute->SetIsMuted(false);
    }
}

} // namespace se_audio

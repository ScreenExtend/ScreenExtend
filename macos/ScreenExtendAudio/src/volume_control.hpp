// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).
//
// Volume / mute wiring for the ScreenExtend Audio device (PRD-macos-legacy-audio.md §6.2, layer 1).
//
// libASPL's VolumeControl / MuteControl objects (added by Device::AddStreamWithControlsAsync) are
// what make the device expose kAudioDevicePropertyVolumeScalar / kAudioDevicePropertyMute, which
// is what tells macOS "this output device has a volume control" and re-enables the hardware volume
// keys + menu-bar slider. This file only sets sane initial state; the value the user dials is read
// and *applied* host-side (`macos_utils/audio/legacy/volume_proxy.rs`, PRD §6.2 layer 2) — the
// driver never attenuates the captured audio itself (that would couple local volume to the stream).

#pragma once

#include <memory>

namespace aspl {
class Device;
} // namespace aspl

namespace se_audio {

// Set the output volume control to unity and the mute control to unmuted. Called once from
// BuildDevice(), off the realtime path.
void ConfigureVolumeAndMute(const std::shared_ptr<aspl::Device>& device);

} // namespace se_audio

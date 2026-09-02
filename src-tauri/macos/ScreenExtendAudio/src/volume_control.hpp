// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).

#pragma once

#include <memory>

namespace aspl {
class Device;
} // namespace aspl

namespace se_audio {

void ConfigureVolumeAndMute(const std::shared_ptr<aspl::Device>& device);

} // namespace se_audio

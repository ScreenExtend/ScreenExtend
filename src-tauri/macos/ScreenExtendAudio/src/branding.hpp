// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).
//
// single source of truth for the driver's branded strings; nothing branded is hardcoded elsewhere

#pragma once

namespace se_audio::branding {

inline constexpr char kBundleIdentifier[] = "app.screenextend.desktop.audio";
inline constexpr char kModelUID[]         = "app.screenextend.desktop.audio";

inline constexpr char kDeviceName[]   = "ScreenExtend Audio";
inline constexpr char kDeviceUID[]    = "app.screenextend.desktop.audio.device";
inline constexpr char kManufacturer[] = "ScreenExtend";
inline constexpr char kBoxName[]      = "ScreenExtend Audio";

inline constexpr char kFactoryUUID[] = "70526F9B-C4FF-4A0E-883C-6805143705DB";

inline constexpr char kShmName[] = "/ScreenExtendAudio";

} // namespace se_audio::branding

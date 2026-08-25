// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).
//
// SINGLE SOURCE OF TRUTH for every user-visible / bundle-namespace string in the driver
// (PRD-macos-legacy-audio.md §3). Nothing branded may be hardcoded anywhere else in the driver;
// grep this file to audit the naming rules. Every identifier here is derived from the app's real
// bundle identifier in `src-tauri/tauri.conf.json` (`app.screenextend.desktop`) — not invented.
//
// FORBIDDEN anywhere in this tree (PRD §3): the names of other virtual-audio products. The only
// third-party name permitted in the repo is the libASPL/MIT attribution in the acknowledgements
// file — never here, never in a device name, bundle id, or the installer.

#pragma once

namespace se_audio::branding {

// ── Bundle namespace (derived from app id `app.screenextend.desktop`) ────────────────────────
inline constexpr char kBundleIdentifier[] = "app.screenextend.desktop.audio";
inline constexpr char kModelUID[]         = "app.screenextend.desktop.audio";

// ── Device identity shown in System Preferences → Sound ──────────────────────────────────────
inline constexpr char kDeviceName[]   = "ScreenExtend Audio";
inline constexpr char kDeviceUID[]    = "app.screenextend.desktop.audio.device";
inline constexpr char kManufacturer[] = "ScreenExtend";
inline constexpr char kBoxName[]      = "ScreenExtend Audio";

// ── CFPlugIn factory UUID (unique to this driver; regenerate only for a hard ABI break) ──────
// Must match CFPlugInFactories / CFPlugInTypes in Info.plist and the entry-point symbol below.
inline constexpr char kFactoryUUID[] = "70526F9B-C4FF-4A0E-883C-6805143705DB";

// ── Shared-memory transport (PRD §5.2a) ──────────────────────────────────────────────────────
// POSIX shm names are capped at PSHMNAMLEN (31 chars incl. the leading slash). The full bundle id
// would overflow, so the transport uses this short fixed name. The Rust reader
// (`macos_utils/audio/legacy/shm_reader.rs`) MUST use the identical constant.
inline constexpr char kShmName[] = "/ScreenExtendAudio"; // 18 chars

} // namespace se_audio::branding

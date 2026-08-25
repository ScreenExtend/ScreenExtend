//! The Rust mirror of `macos/ScreenExtendAudio/src/branding.hpp` — the branded strings the host
//! side needs to find and talk to the driver (PRD-macos-legacy-audio.md §3). Keep in sync with the
//! C++ header; these are the only place the device UID / install path appear in Rust.

/// Device UID advertised by the driver; used to locate the device in the HAL device list.
pub const DEVICE_UID: &str = "app.screenextend.desktop.audio.device";

/// Human-readable device name shown in System Preferences → Sound.
pub const DEVICE_NAME: &str = "ScreenExtend Audio";

/// Driver bundle identifier (matches Info.plist CFBundleIdentifier).
pub const BUNDLE_IDENTIFIER: &str = "app.screenextend.desktop.audio";

/// Where the notarized `.pkg` installs the driver (root-owned HAL plug-ins directory).
pub const INSTALL_PATH: &str = "/Library/Audio/Plug-Ins/HAL/ScreenExtendAudio.driver";

/// Privileged helper identifier (SMJobBless), if/when a persistent helper is used.
pub const HELPER_IDENTIFIER: &str = "app.screenextend.desktop.audiohelper";

/// POSIX shared-memory transport name (mirrors `branding.hpp` `kShmName`). ≤ 31 chars (PSHMNAMLEN).
pub const SHM_NAME: &str = "/ScreenExtendAudio";

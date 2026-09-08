// Copyright (c) ScreenExtend authors
// Licensed under AGPL-3.0 (see repo root LICENSE).
//
// A HAL plug-in runs inside coreaudiod, where there is no stdout and no debugger worth the name.
// `log stream --predicate 'subsystem == "app.screenextend.desktop.audio"'` is the only window into
// it, so everything that can go wrong at load or open time says so here.
//
// os_log is not real-time safe; nothing in this header may be called from an IO callback.

#pragma once

#include "branding.hpp"

#include <os/log.h>

namespace se_audio {

inline os_log_t Log()
{
    static os_log_t log = os_log_create(branding::kBundleIdentifier, "driver");
    return log;
}

} // namespace se_audio

#define SE_LOG(fmt, ...) os_log(se_audio::Log(), fmt, ##__VA_ARGS__)
#define SE_LOG_ERROR(fmt, ...) os_log_error(se_audio::Log(), fmt, ##__VA_ARGS__)

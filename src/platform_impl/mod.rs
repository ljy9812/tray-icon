// Copyright 2022-2022 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

#[cfg(target_os = "windows")]
#[path = "windows/mod.rs"]
mod platform;
#[cfg(all(
    any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ),
    not(target_env = "ohos")
))]
#[path = "gtk/mod.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos/mod.rs"]
mod platform;
#[cfg(target_env = "ohos")]
#[path = "ohos/mod.rs"]
mod platform;

pub(crate) use self::platform::*;

#[cfg(target_env = "ohos")]
pub use self::platform::send_icon_click;

/// Public re-export of the cached last status-bar bridge failure, surfaced
/// through [`crate::last_bridge_error`] for embedder diagnostics.
#[cfg(target_env = "ohos")]
pub use self::platform::last_bridge_error;

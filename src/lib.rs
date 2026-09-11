// Copyright 2022-2022 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

#![allow(clippy::uninlined_format_args)]

//! tray-icon lets you create tray icons for desktop applications.
//!
//! # Platforms supported:
//!
//! - Windows
//! - macOS
//! - Linux (gtk Only)
//!
//! # Platform-specific notes:
//!
//! - On Windows and Linux, an event loop must be running on the thread, on Windows, a win32 event loop and on Linux, a gtk event loop. It doesn't need to be the main thread but you have to create the tray icon on the same thread as the event loop.
//! - On macOS, an event loop must be running on the main thread so you also need to create the tray icon on the main thread. You must make sure that the event loop is already running and not just created before creating a TrayIcon to prevent issues with fullscreen apps. In Winit for example the earliest you can create icons is on [`StartCause::Init`](https://docs.rs/winit/latest/winit/event/enum.StartCause.html#variant.Init).
//!
//! # Dependencies (Linux Only)
//!
//! On Linux, `gtk`, `libxdo` is used to make the predfined `Copy`, `Cut`, `Paste` and `SelectAll` menu items work and `libappindicator` or `libayatnat-appindicator` are used to create the tray icon, so make sure to install them on your system.
//!
//! #### Arch Linux / Manjaro:
//!
//! ```sh
//! pacman -S gtk3 xdotool libappindicator-gtk3 #or libayatana-appindicator
//! ```
//!
//! #### Debian / Ubuntu:
//!
//! ```sh
//! sudo apt install libgtk-3-dev libxdo-dev libappindicator3-dev #or libayatana-appindicator3-dev
//! ```
//!
//! # Examples
//!
//! #### Create a tray icon without a menu.
//!
//! ```no_run
//! use tray_icon::{TrayIconBuilder, Icon};
//!
//! # let icon = Icon::from_rgba(Vec::new(), 0, 0).unwrap();
//! let tray_icon = TrayIconBuilder::new()
//!     .with_tooltip("system-tray - tray icon library!")
//!     .with_icon(icon)
//!     .build()
//!     .unwrap();
//! ```
//!
//! #### Create a tray icon with a menu.
//!
//! ```no_run
//! use tray_icon::{TrayIconBuilder, menu::Menu,Icon};
//!
//! # let icon = Icon::from_rgba(Vec::new(), 0, 0).unwrap();
//! let tray_menu = Menu::new();
//! let tray_icon = TrayIconBuilder::new()
//!     .with_menu(Box::new(tray_menu))
//!     .with_tooltip("system-tray - tray icon library!")
//!     .with_icon(icon)
//!     .build()
//!     .unwrap();
//! ```
//!
//! # Processing tray events
//!
//! You can use [`TrayIconEvent::receiver`] to get a reference to the [`TrayIconEventReceiver`]
//! which you can use to listen to events when a click happens on the tray icon
//! ```no_run
//! use tray_icon::TrayIconEvent;
//!
//! if let Ok(event) = TrayIconEvent::receiver().try_recv() {
//!     println!("{:?}", event);
//! }
//! ```
//!
//! You can also listen for the menu events using [`MenuEvent::receiver`](crate::menu::MenuEvent::receiver) to get events for the tray context menu.
//!
//! ```no_run
//! use tray_icon::{TrayIconEvent, menu::MenuEvent};
//!
//! if let Ok(event) = TrayIconEvent::receiver().try_recv() {
//!     println!("tray event: {:?}", event);
//! }
//!
//! if let Ok(event) = MenuEvent::receiver().try_recv() {
//!     println!("menu event: {:?}", event);
//! }
//! ```
//!
//! ### Note for [winit] or [tao] users:
//!
//! You should use [`TrayIconEvent::set_event_handler`] and forward
//! the tray icon events to the event loop by using [`EventLoopProxy`]
//! so that the event loop is awakened on each tray icon event.
//! Same can be done for menu events using [`MenuEvent::set_event_handler`].
//!
//! ```no_run
//! # use winit::event_loop::EventLoop;
//! enum UserEvent {
//!   TrayIconEvent(tray_icon::TrayIconEvent),
//!   MenuEvent(tray_icon::menu::MenuEvent)
//! }
//!
//! let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
//!
//! let proxy = event_loop.create_proxy();
//! tray_icon::TrayIconEvent::set_event_handler(Some(move |event| {
//!     proxy.send_event(UserEvent::TrayIconEvent(event));
//! }));
//!
//! let proxy = event_loop.create_proxy();
//! tray_icon::menu::MenuEvent::set_event_handler(Some(move |event| {
//!     proxy.send_event(UserEvent::MenuEvent(event));
//! }));
//! ```
//!
//! [`EventLoopProxy`]: https://docs.rs/winit/latest/winit/event_loop/struct.EventLoopProxy.html
//! [winit]: https://docs.rs/winit
//! [tao]: https://docs.rs/tao

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

use counter::Counter;
use crossbeam_channel::{unbounded, Receiver, Sender};
use once_cell::sync::{Lazy, OnceCell};

mod counter;
mod error;
mod icon;
mod platform_impl;
mod tray_icon_id;

pub use self::error::*;
pub use self::icon::{BadIcon, Icon};
pub use self::tray_icon_id::TrayIconId;

/// Set the OHOS app context for tray icon initialization.
///
/// This must be called before creating any tray icons on OpenHarmony.
/// Tauri calls this automatically during app initialization.
#[cfg(target_env = "ohos")]
pub fn set_ohos_app(app: openharmony_ability::OpenHarmonyApp) {
    platform_impl::set_ohos_app(app);
}

/// Returns the last OHOS status-bar bridge error (if any).
///
/// Bridge calls are dispatched to a worker thread (fire-and-forget — blocking
/// the caller would deadlock the TSFN event loop), so failures cannot be
/// returned through the tray API's `Result`s; they are cached by the worker
/// and exposed here for embedder (tauri) diagnostics.
#[cfg(target_env = "ohos")]
pub fn last_bridge_error() -> Option<crate::Error> {
    platform_impl::last_bridge_error().map(crate::Error::OhosError)
}

/// Sends an icon click event into tray-icon's internal channel (for test
/// simulation). Not part of the public API surface — kept exported
/// (hidden from docs) for the frontend automation suite.
#[cfg(target_env = "ohos")]
#[doc(hidden)]
pub use platform_impl::send_icon_click;

/// Re-export of [muda](::muda) crate and used for tray context menu.
pub mod menu {
    pub use muda::*;
}
pub use muda::dpi;

static COUNTER: Counter = Counter::new();

/// QuickOperation configuration for OHOS left-click popup. **OHOS only**.
///
/// On OHOS, when a user left-clicks the tray icon, the system can show a popup panel
/// rendered by a `StatusBarViewExtensionAbility`. This struct configures that popup.
///
/// On other platforms, this is silently ignored.
#[derive(Debug, Clone)]
pub struct QuickOperationConfig {
    /// Popup title bar text. Overlong text shows ellipsis.
    pub title: String,

    /// Popup content area height in vp (must be > 0).
    pub height: u32,

    /// Name of the `StatusBarViewExtensionAbility` registered in `module.json5`.
    /// Empty string disables the popup (left-click only fires `statusBarIconClick` event).
    pub ability_name: String,

    /// Module name where the ExtensionAbility is defined.
    /// `None` uses the default module.
    pub module_name: Option<String>,

    /// Whether to show a loading animation.
    /// Requires OHOS 6.0.0(20)+. `None` uses system default.
    pub loading_status: Option<bool>,
}

impl Default for QuickOperationConfig {
    fn default() -> Self {
        Self {
            title: String::new(),
            height: 200,
            ability_name: String::new(),
            module_name: None,
            loading_status: None,
        }
    }
}

/// Attributes to use when creating a tray icon.
pub struct TrayIconAttributes {
    /// Tray icon tooltip
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Unsupported.
    pub tooltip: Option<String>,

    /// Tray menu
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux**: once a menu is set, it cannot be removed.
    pub menu: Option<Box<dyn menu::ContextMenu>>,

    /// Tray icon
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Sometimes the icon won't be visible unless a menu is set.
    ///   Setting an empty [`Menu`](crate::menu::Menu) is enough.
    pub icon: Option<Icon>,

    /// Tray icon temp dir path. **Linux only**.
    pub temp_dir_path: Option<PathBuf>,

    /// Use the icon as a [template](https://developer.apple.com/documentation/appkit/nsimage/1520017-template?language=objc). **macOS only**.
    pub icon_is_template: bool,

    /// Whether to show the tray menu on left click or not, default is `true`.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Unsupported.
    pub menu_on_left_click: bool,

    /// Whether to show the tray menu on right click or not, default is `true`.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Unsupported.
    pub menu_on_right_click: bool,

    /// Tray icon title.
    ///
    /// ## Platform-specific
    ///
    /// - **Linux:** The title will not be shown unless there is an icon
    ///   as well.  The title is useful for numerical and other frequently
    ///   updated information.  In general, it shouldn't be shown unless a
    ///   user requests it as it can take up a significant amount of space
    ///   on the user's panel.  This may not be shown in all visualizations.
    /// - **Windows:** Unsupported.
    pub title: Option<String>,

    /// QuickOperation configuration for left-click popup. **OHOS only**.
    ///
    /// On OHOS, configures the system popup panel shown when the user left-clicks
    /// the tray icon. On other platforms, this field is silently ignored.
    pub quick_operation: Option<QuickOperationConfig>,
}

impl Default for TrayIconAttributes {
    fn default() -> Self {
        Self {
            tooltip: None,
            menu: None,
            icon: None,
            temp_dir_path: None,
            icon_is_template: false,
            menu_on_left_click: true,
            menu_on_right_click: true,
            title: None,
            quick_operation: None,
        }
    }
}

/// [`TrayIcon`] builder struct and associated methods.
#[derive(Default)]
pub struct TrayIconBuilder {
    id: TrayIconId,
    attrs: TrayIconAttributes,
}

impl TrayIconBuilder {
    /// Creates a new [`TrayIconBuilder`] with default [`TrayIconAttributes`].
    ///
    /// See [`TrayIcon::new`] for more info.
    pub fn new() -> Self {
        Self {
            id: TrayIconId::new_unique(),
            attrs: TrayIconAttributes::default(),
        }
    }

    /// Sets the unique id to build the tray icon with.
    pub fn with_id<I: Into<TrayIconId>>(mut self, id: I) -> Self {
        self.id = id.into();
        self
    }

    /// Set the a menu for this tray icon.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux**: once a menu is set, it cannot be removed or replaced but you can change its content.
    pub fn with_menu(mut self, menu: Box<dyn menu::ContextMenu>) -> Self {
        self.attrs.menu = Some(menu);
        self
    }

    /// Set an icon for this tray icon.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Sometimes the icon won't be visible unless a menu is set.
    ///   Setting an empty [`Menu`](crate::menu::Menu) is enough.
    pub fn with_icon(mut self, icon: Icon) -> Self {
        self.attrs.icon = Some(icon);
        self
    }

    /// Set a tooltip for this tray icon.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Unsupported.
    pub fn with_tooltip<S: AsRef<str>>(mut self, s: S) -> Self {
        self.attrs.tooltip = Some(s.as_ref().to_string());
        self
    }

    /// Set the tray icon title.
    ///
    /// ## Platform-specific
    ///
    /// - **Linux:** The title will not be shown unless there is an icon
    ///   as well.  The title is useful for numerical and other frequently
    ///   updated information.  In general, it shouldn't be shown unless a
    ///   user requests it as it can take up a significant amount of space
    ///   on the user's panel.  This may not be shown in all visualizations.
    /// - **Windows:** Unsupported.
    pub fn with_title<S: AsRef<str>>(mut self, title: S) -> Self {
        self.attrs.title.replace(title.as_ref().to_string());
        self
    }

    /// Set tray icon temp dir path. **Linux only**.
    ///
    /// On Linux, we need to write the icon to the disk and usually it will
    /// be `$XDG_RUNTIME_DIR/tray-icon` or `$TEMP/tray-icon`.
    pub fn with_temp_dir_path<P: AsRef<Path>>(mut self, s: P) -> Self {
        self.attrs.temp_dir_path = Some(s.as_ref().to_path_buf());
        self
    }

    /// Use the icon as a [template](https://developer.apple.com/documentation/appkit/nsimage/1520017-template?language=objc).
    ///
    /// ## Platform-specific
    ///
    /// - **macOS**: Template image rendered by system based on appearance (dark/light mode).
    /// - **OHOS**: Generates white and black versions from alpha mask; system selects based on wallpaper color.
    /// - **Windows / Linux**: Unsupported.
    pub fn with_icon_as_template(mut self, is_template: bool) -> Self {
        self.attrs.icon_is_template = is_template;
        self
    }

    /// Whether to show the tray menu on left click or not, default is `true`.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Unsupported.
    pub fn with_menu_on_left_click(mut self, enable: bool) -> Self {
        self.attrs.menu_on_left_click = enable;
        self
    }

    /// Whether to show the tray menu on right click or not, default is `true`.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Unsupported.
    pub fn with_menu_on_right_click(mut self, enable: bool) -> Self {
        self.attrs.menu_on_right_click = enable;
        self
    }

    /// Set QuickOperation for left-click popup. **OHOS only**.
    ///
    /// On OHOS, configures the system popup panel shown when the user left-clicks
    /// the tray icon. The popup is rendered by a `StatusBarViewExtensionAbility`
    /// that the application registers in `module.json5`.
    ///
    /// On other platforms, this is silently ignored.
    pub fn with_quick_operation(mut self, config: QuickOperationConfig) -> Self {
        self.attrs.quick_operation = Some(config);
        self
    }

    /// Returns the configured [`QuickOperationConfig`], if any. **OHOS only** —
    /// embedders (tauri) use this to decide whether to layer their own
    /// app-specific defaults on top.
    pub fn quick_operation_config(&self) -> Option<&QuickOperationConfig> {
        self.attrs.quick_operation.as_ref()
    }

    /// Access the unique id that will be assigned to the tray icon
    /// this builder will create.
    pub fn id(&self) -> &TrayIconId {
        &self.id
    }

    /// Builds and adds a new [`TrayIcon`] to the system tray.
    pub fn build(self) -> Result<TrayIcon> {
        TrayIcon::with_id(self.id, self.attrs)
    }
}

/// Tray icon struct and associated methods.
///
/// This type is reference-counted and the icon is removed when the last instance is dropped.
#[derive(Clone)]
pub struct TrayIcon {
    id: TrayIconId,
    tray: Rc<RefCell<platform_impl::TrayIcon>>,
}

impl TrayIcon {
    /// Builds and adds a new tray icon to the system tray.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Sometimes the icon won't be visible unless a menu is set.
    ///   Setting an empty [`Menu`](crate::menu::Menu) is enough.
    pub fn new(attrs: TrayIconAttributes) -> Result<Self> {
        let id = TrayIconId::new_unique();
        Ok(Self {
            tray: Rc::new(RefCell::new(platform_impl::TrayIcon::new(
                id.clone(),
                attrs,
            )?)),
            id,
        })
    }

    /// Builds and adds a new tray icon to the system tray with the specified Id.
    ///
    /// See [`TrayIcon::new`] for more info.
    pub fn with_id<I: Into<TrayIconId>>(id: I, attrs: TrayIconAttributes) -> Result<Self> {
        let id = id.into();
        Ok(Self {
            tray: Rc::new(RefCell::new(platform_impl::TrayIcon::new(
                id.clone(),
                attrs,
            )?)),
            id,
        })
    }

    /// Returns the id associated with this tray icon.
    pub fn id(&self) -> &TrayIconId {
        &self.id
    }

    /// Set new tray icon. If `None` is provided, it will remove the icon.
    pub fn set_icon(&self, icon: Option<Icon>) -> Result<()> {
        self.tray.borrow_mut().set_icon(icon)
    }

    /// Set new tray menu.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux**: once a menu is set it cannot be removed so `None` has no effect
    pub fn set_menu(&self, menu: Option<Box<dyn menu::ContextMenu>>) {
        self.tray.borrow_mut().set_menu(menu)
    }

    /// Sets the tooltip for this tray icon.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Unsupported
    pub fn set_tooltip<S: AsRef<str>>(&self, tooltip: Option<S>) -> Result<()> {
        self.tray.borrow_mut().set_tooltip(tooltip)
    }

    /// Sets the tooltip for this tray icon.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** The title will not be shown unless there is an icon
    ///   as well.  The title is useful for numerical and other frequently
    ///   updated information.  In general, it shouldn't be shown unless a
    ///   user requests it as it can take up a significant amount of space
    ///   on the user's panel.  This may not be shown in all visualizations.
    /// - **Windows:** Unsupported
    pub fn set_title<S: AsRef<str>>(&self, title: Option<S>) {
        self.tray.borrow_mut().set_title(title)
    }

    /// Show or hide this tray icon
    pub fn set_visible(&self, visible: bool) -> Result<()> {
        self.tray.borrow_mut().set_visible(visible)
    }

    /// Sets the tray icon temp dir path. **Linux only**.
    ///
    /// On Linux, we need to write the icon to the disk and usually it will
    /// be `$XDG_RUNTIME_DIR/tray-icon` or `$TEMP/tray-icon`.
    pub fn set_temp_dir_path<P: AsRef<Path>>(&self, path: Option<P>) {
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        self.tray.borrow_mut().set_temp_dir_path(path);
        #[cfg(not(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        )))]
        let _ = path;
    }

    /// Set the current icon as a [template](https://developer.apple.com/documentation/appkit/nsimage/1520017-template?language=objc).
    ///
    /// ## Platform-specific
    ///
    /// - **macOS**: Template image rendered by system based on appearance (dark/light mode).
    /// - **OHOS**: Generates white and black versions from alpha mask; system selects based on wallpaper color.
    /// - **Windows / Linux**: Unsupported.
    pub fn set_icon_as_template(&self, is_template: bool) {
        #[cfg(target_os = "macos")]
        self.tray.borrow_mut().set_icon_as_template(is_template);
        #[cfg(target_env = "ohos")]
        let _ = self.tray.borrow_mut().set_icon_as_template(is_template);
        #[cfg(not(any(target_os = "macos", target_env = "ohos")))]
        let _ = is_template;
    }

    pub fn set_icon_with_as_template(&self, icon: Option<Icon>, is_template: bool) -> Result<()> {
        #[cfg(any(target_os = "macos", target_env = "ohos"))]
        return self
            .tray
            .borrow_mut()
            .set_icon_with_as_template(icon, is_template);
        #[cfg(not(any(target_os = "macos", target_env = "ohos")))]
        {
            let _ = icon;
            let _ = is_template;
            Ok(())
        }
    }

    /// Disable or enable showing the tray menu on left click.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Unsupported.
    pub fn set_show_menu_on_left_click(&self, enable: bool) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        self.tray.borrow_mut().set_show_menu_on_left_click(enable);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let _ = enable;
    }

    /// Disable or enable showing the tray menu on right click.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Unsupported.
    pub fn set_show_menu_on_right_click(&self, enable: bool) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        self.tray.borrow_mut().set_show_menu_on_right_click(enable);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let _ = enable;
    }

    /// Manually show the tray menu at the current cursor position.
    ///
    /// This is useful when you want to control when the menu is displayed,
    /// for example after updating menu items dynamically.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux:** Unsupported.
    /// - **OHOS:** Unsupported. statusBarManager has no API to programmatically trigger the menu.
    pub fn show_menu(&self) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        self.tray.borrow().show_menu();
    }

    /// Set QuickOperation for left-click popup. **OHOS only**.
    ///
    /// On OHOS, configures the system popup panel shown when the user left-clicks
    /// the tray icon. Pass `None` to disable the popup (left-click will only fire events).
    ///
    /// On other platforms, this is silently ignored.
    pub fn set_quick_operation(&self, config: Option<QuickOperationConfig>) {
        #[cfg(target_env = "ohos")]
        self.tray.borrow_mut().set_quick_operation(config);
        #[cfg(not(target_env = "ohos"))]
        let _ = config;
    }

    /// Get tray icon rect.
    ///
    /// ## Platform-specific:
    ///
    /// - **Linux**: Unsupported.
    pub fn rect(&self) -> Option<Rect> {
        self.tray.borrow().rect()
    }

    /// Get the tray icon's underlying [window handle](windows_sys::Win32::Foundation::HWND) **Windows only**.
    ///
    /// This window handle is valid as long as the tray icon.
    #[cfg(windows)]
    pub fn window_handle(&self) -> windows_sys::Win32::Foundation::HWND {
        self.tray.borrow().hwnd()
    }

    /// Get the tray icon's underlying [NSStatusItem](objc2_app_kit::NSStatusItem) **macOS only**.
    ///
    /// Returns `None` if the status item is not available.
    #[cfg(target_os = "macos")]
    pub fn ns_status_item(&self) -> Option<objc2::rc::Retained<objc2_app_kit::NSStatusItem>> {
        self.tray.borrow().ns_status_item().cloned()
    }

    /// Get the tray icon's underlying [AppIndicator](libappindicator::AppIndicator) **Linux only**.
    ///
    /// # Safety
    ///
    /// The returned pointer is valid as long as the `TrayIcon` is.
    #[cfg(all(unix, not(target_os = "macos"), not(target_env = "ohos")))]
    pub unsafe fn app_indicator(&self) -> *const libappindicator::AppIndicator {
        self.tray.borrow().app_indicator() as *const _
    }
}

/// Describes a tray icon event.
///
/// ## Platform-specific:
///
/// - **Linux**: Unsupported. The event is not emmited even though the icon is shown
///   and will still show a context menu on right click.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type"))]
#[non_exhaustive]
pub enum TrayIconEvent {
    /// A click happened on the tray icon.
    #[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
    Click {
        /// Id of the tray icon which triggered this event.
        id: TrayIconId,
        /// Physical Position of this event.
        position: dpi::PhysicalPosition<f64>,
        /// Position and size of the tray icon.
        rect: Rect,
        /// Mouse button that triggered this event.
        button: MouseButton,
        /// Mouse button state when this event was triggered.
        button_state: MouseButtonState,
    },
    /// A double click happened on the tray icon. **Windows Only**
    DoubleClick {
        /// Id of the tray icon which triggered this event.
        id: TrayIconId,
        /// Physical Position of this event.
        position: dpi::PhysicalPosition<f64>,
        /// Position and size of the tray icon.
        rect: Rect,
        /// Mouse button that triggered this event.
        button: MouseButton,
    },
    /// The mouse entered the tray icon region.
    Enter {
        /// Id of the tray icon which triggered this event.
        id: TrayIconId,
        /// Physical Position of this event.
        position: dpi::PhysicalPosition<f64>,
        /// Position and size of the tray icon.
        rect: Rect,
    },
    /// The mouse moved over the tray icon region.
    Move {
        /// Id of the tray icon which triggered this event.
        id: TrayIconId,
        /// Physical Position of this event.
        position: dpi::PhysicalPosition<f64>,
        /// Position and size of the tray icon.
        rect: Rect,
    },
    /// The mouse left the tray icon region.
    Leave {
        /// Id of the tray icon which triggered this event.
        id: TrayIconId,
        /// Physical Position of this event.
        position: dpi::PhysicalPosition<f64>,
        /// Position and size of the tray icon.
        rect: Rect,
    },
}

/// Describes the mouse button state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Default)]
pub enum MouseButtonState {
    #[default]
    Up,
    Down,
}

/// Describes which mouse button triggered the event..
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Default)]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

/// Describes a rectangle including position (x - y axis) and size.
#[derive(Debug, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Rect {
    pub size: dpi::PhysicalSize<u32>,
    pub position: dpi::PhysicalPosition<f64>,
}

impl Default for Rect {
    fn default() -> Self {
        Self {
            size: dpi::PhysicalSize::new(0, 0),
            position: dpi::PhysicalPosition::new(0., 0.),
        }
    }
}

/// A reciever that could be used to listen to tray events.
pub type TrayIconEventReceiver = Receiver<TrayIconEvent>;
type TrayIconEventHandler = Box<dyn Fn(TrayIconEvent) + Send + Sync + 'static>;

static TRAY_CHANNEL: Lazy<(Sender<TrayIconEvent>, TrayIconEventReceiver)> = Lazy::new(unbounded);
static TRAY_EVENT_HANDLER: OnceCell<Option<TrayIconEventHandler>> = OnceCell::new();

impl TrayIconEvent {
    /// Returns the id of the tray icon which triggered this event.
    pub fn id(&self) -> &TrayIconId {
        match self {
            TrayIconEvent::Click { id, .. } => id,
            TrayIconEvent::DoubleClick { id, .. } => id,
            TrayIconEvent::Enter { id, .. } => id,
            TrayIconEvent::Move { id, .. } => id,
            TrayIconEvent::Leave { id, .. } => id,
        }
    }

    /// Gets a reference to the event channel's [`TrayIconEventReceiver`]
    /// which can be used to listen for tray events.
    ///
    /// ## Note
    ///
    /// This will not receive any events if [`TrayIconEvent::set_event_handler`] has been called with a `Some` value.
    pub fn receiver<'a>() -> &'a TrayIconEventReceiver {
        &TRAY_CHANNEL.1
    }

    /// Set a handler to be called for new events. Useful for implementing custom event sender.
    ///
    /// ## Note
    ///
    /// Calling this function with a `Some` value,
    /// will not send new events to the channel associated with [`TrayIconEvent::receiver`]
    pub fn set_event_handler<F: Fn(TrayIconEvent) + Send + Sync + 'static>(f: Option<F>) {
        if let Some(f) = f {
            let _ = TRAY_EVENT_HANDLER.set(Some(Box::new(f)));
        } else {
            let _ = TRAY_EVENT_HANDLER.set(None);
        }
    }

    #[allow(unused)]
    pub(crate) fn send(event: TrayIconEvent) {
        if let Some(handler) = TRAY_EVENT_HANDLER.get_or_init(|| None) {
            handler(event);
        } else {
            let _ = TRAY_CHANNEL.0.send(event);
        }
    }
}

#[cfg(test)]
mod tests {

    #[cfg(feature = "serde")]
    #[test]
    fn it_serializes() {
        use super::*;
        let event = TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Down,
            id: TrayIconId::new("id"),
            position: dpi::PhysicalPosition::default(),
            rect: Rect::default(),
        };

        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "Click",
                "button": "Left",
                "buttonState": "Down",
                "id": "id",
                "position": {
                    "x": 0.0,
                    "y": 0.0,
                },
                "rect": {
                    "size": {
                        "width": 0,
                        "height": 0,
                    },
                    "position": {
                        "x": 0.0,
                        "y": 0.0,
                    },
                }
            })
        )
    }

    #[test]
    fn quick_operation_config_default() {
        use super::*;
        let config = QuickOperationConfig::default();
        assert!(config.title.is_empty());
        assert_eq!(config.height, 200);
        assert!(config.ability_name.is_empty());
        assert!(config.module_name.is_none());
        assert!(config.loading_status.is_none());
    }

    #[test]
    fn builder_with_quick_operation() {
        use super::*;
        let config = QuickOperationConfig {
            title: "Test Title".into(),
            height: 300,
            ability_name: "TestAbility".into(),
            module_name: Some("entry".into()),
            loading_status: Some(true),
        };
        let builder = TrayIconBuilder::new().with_quick_operation(config);
        let qo = builder.attrs.quick_operation.as_ref().unwrap();
        assert_eq!(qo.title, "Test Title");
        assert_eq!(qo.height, 300);
        assert_eq!(qo.ability_name, "TestAbility");
        assert_eq!(qo.module_name, Some("entry".into()));
        assert_eq!(qo.loading_status, Some(true));
    }
}

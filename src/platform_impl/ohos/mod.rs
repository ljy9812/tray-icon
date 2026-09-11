mod event;
mod icon;

pub(crate) use icon::PlatformIcon;
pub use event::send_icon_click;

use crate::{TrayIconAttributes, TrayIconId};
use once_cell::sync::OnceCell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use openharmony_ability_plugin_statusbar::{
    QuickOperation, StatusBarAddRequest, StatusBarClient, StatusBarIcon, StatusBarItem,
    StatusBarMenuAction, StatusBarMenuItem, StatusBarMenuItemOptions, StatusBarRemoveRequest,
    StatusBarSubMenuItem,
};

static OHOS_APP: OnceCell<openharmony_ability::OpenHarmonyApp> = OnceCell::new();
static STATUSBAR_CLIENT: OnceCell<StatusBarClient> = OnceCell::new();

/// Process-level "a tray is currently registered with the status bar" mirror.
/// OHOS allows only one status-bar tray per ability (a second addToStatusBar
/// is rejected with 16000078 "Multi-instance is not supported"), and this
/// backend keeps process-global state (MENU_METADATA, event::TRAY_ID) — a
/// second visible tray would silently shadow the first. `TrayIcon::new`
/// therefore refuses to create one instead.
static TRAY_VISIBLE: AtomicBool = AtomicBool::new(false);

pub(crate) static MENU_METADATA: once_cell::sync::Lazy<Mutex<MenuMetadata>> =
    once_cell::sync::Lazy::new(|| Mutex::new(MenuMetadata::default()));

#[derive(Default)]
pub(crate) struct MenuMetadata {
    pub predefined_map: HashMap<String, String>,
    /// About metadata by item id — carried on the execute-predefined request
    /// so the ArkTS About panel shows the muda `AboutMetadata`. No check-state
    /// mirror is kept: muda's CHECK_ITEMS is the single check-state source
    /// (see `muda::register_check_items` / `muda::toggle_check_item`).
    pub about_metadata: HashMap<String, AboutMetadataJson>,
    pub menu_json: Option<String>,
    pub flat_ids: Vec<String>,
}

pub fn set_ohos_app(app: openharmony_ability::OpenHarmonyApp) {
    // Register the Rust-side bridge plugins BEFORE creating the clients that
    // dispatch calls through them. `register_plugin` populates the native
    // module's bridge-plugin declarations, which the ArkTS BridgeHost matches
    // against its `bridgePlugins` factories in configurePlugins(). Without
    // registration, every statusbar/menu bridge call rejects with
    // "Bridge plugin 'ohos.statusbar'/'ohos.menu' is not installed for 'api_lib'".
    if let Err(e) = app.register_plugin(openharmony_ability_plugin_statusbar::StatusBarBridgePlugin) {
        log::error!("[TrayIcon] failed to register StatusBarBridgePlugin: {}", e);
    }
    if let Err(e) = app.register_plugin(openharmony_ability_plugin_menu::MenuBridgePlugin) {
        log::error!("[TrayIcon] failed to register MenuBridgePlugin: {}", e);
    }
    let statusbar_client = StatusBarClient::new(&app)
        .expect("Failed to create StatusBarClient");
    let menu_client = openharmony_ability_plugin_menu::MenuClient::new(&app)
        .expect("Failed to create MenuClient");
    OHOS_APP.set(app).expect("OHOS_APP already set");
    if STATUSBAR_CLIENT.set(statusbar_client).is_err() {
        panic!("STATUSBAR_CLIENT already set");
    }
    // Inject muda's MenuClient (muda does not hold OpenHarmonyApp itself)
    muda::set_menu_client(menu_client);
    // Register tray-icon's local event channels with plugin-statusbar so
    // bridge events flow into the channels owned by tray-icon.
    event::register_statusbar_channels();
}

// ─── Last bridge error (diagnostics for fire-and-forget calls) ───────────────
// Bridge calls are dispatched to the worker thread below; their results
// cannot be returned without blocking the caller, so the worker caches the
// last failure here and `tray_icon::last_bridge_error()` exposes it for
// embedder (tauri) diagnostics.

static LAST_BRIDGE_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Returns the last status-bar bridge error (if any), without clearing it.
pub fn last_bridge_error() -> Option<String> {
    LAST_BRIDGE_ERROR.lock().unwrap().clone()
}

/// Records a bridge failure from the worker thread.
fn record_bridge_error(op: &str, err: &impl std::fmt::Display) {
    log::warn!("[TrayIcon] {} error: {}", op, err);
    *LAST_BRIDGE_ERROR.lock().unwrap() = Some(format!("{}: {}", op, err));
}

/// Runs `f` with the StatusBarClient on the calling (worker) thread. A missing
/// client is recorded as a bridge error instead of panicking: a panicking
/// worker thread dies permanently and silently drops every subsequent tray
/// bridge call.
fn with_statusbar_client(op: &str, f: impl FnOnce(&StatusBarClient)) {
    match STATUSBAR_CLIENT.get() {
        Some(client) => f(client),
        None => {
            log::error!("[TrayIcon] {} skipped: STATUSBAR_CLIENT not initialized", op);
            *LAST_BRIDGE_ERROR.lock().unwrap() =
                Some(format!("{}: STATUSBAR_CLIENT not initialized", op));
        }
    }
}

// ─── Bridge worker thread ───────────────────────────────────────────────────
// All StatusBarClient bridge calls must run on a Rust worker thread, never on
// the ArkTS/N-API main thread. The main thread owns the TSFN queue; blocking it
// with futures_executor::block_on prevents the very TSFN callbacks that deliver
// bridge responses, causing a deadlock (THREAD_BLOCK_3S watchdog).
//
// A single long-lived worker thread serialises all tray bridge operations.
// Tray operations are infrequent (user-initiated) and must be ordered (e.g.
// remove before re-add), so a single sequential FIFO queue is ideal.
type BridgeCommand = Box<dyn FnOnce() + Send + 'static>;

fn bridge_worker_tx() -> &'static std::sync::mpsc::Sender<BridgeCommand> {
    static TX: once_cell::sync::Lazy<std::sync::mpsc::Sender<BridgeCommand>> =
        once_cell::sync::Lazy::new(|| {
            let (tx, rx) = std::sync::mpsc::channel::<BridgeCommand>();
            std::thread::Builder::new()
                .name("tray-bridge".to_string())
                .spawn(move || {
                    while let Ok(cmd) = rx.recv() {
                        cmd();
                    }
                })
                .expect("Failed to spawn tray-bridge worker thread");
            tx
        });
    &TX
}

/// Dispatch a bridge call to the dedicated worker thread (fire-and-forget).
/// Local validation must happen *before* calling this; bridge errors are
/// logged on the worker and never propagate back to the calling thread.
fn dispatch_bridge_call(f: impl FnOnce() + Send + 'static) {
    if bridge_worker_tx().send(Box::new(f)).is_err() {
        log::warn!("[TrayIcon] bridge worker channel closed, call dropped");
    }
}

pub struct TrayIcon {
    attrs: RefCell<TrayIconAttributes>,
    /// Best-effort mirror of the tray icon's visibility state. Updated locally
    /// on `set_visible`/`new`/`drop`, but may go stale after external state
    /// changes (e.g. system-driven removals) until the next status callback
    /// refreshes it.
    is_visible: RefCell<bool>,
}

impl TrayIcon {
    pub fn new(id: TrayIconId, attrs: TrayIconAttributes) -> crate::Result<Self> {
        // OHOS allows a single status-bar tray per ability (a second
        // addToStatusBar is rejected with 16000078 "Multi-instance is not
        // supported"), and this backend keeps process-global state
        // (MENU_METADATA, event::TRAY_ID) — a second visible tray would
        // silently shadow the first. Fail loudly instead.
        if TRAY_VISIBLE.load(Ordering::Acquire) {
            return Err(crate::Error::Unsupported(
                "only one statusbar tray per ability".to_string(),
            ));
        }

        // Make muda's CHECK_ITEMS the single check-state source for this menu:
        // status-bar check clicks flip the user's CheckMenuItem flags through
        // it (muda::toggle_check_item); no local mirror is kept.
        if let Some(menu) = attrs.menu.as_ref().and_then(|m| m.as_menu()) {
            muda::register_check_items(menu);
        }

        // Extract metadata before building the item (menus may be consumed)
        let (predefined_map, about_metadata, menu_json) = extract_menu_metadata(&attrs.menu);

        {
            let mut metadata = MENU_METADATA.lock().unwrap();
            metadata.predefined_map = predefined_map;
            metadata.about_metadata = about_metadata;
            metadata.menu_json = menu_json;
        }

        let mut item = build_item_from_attrs(&attrs)?;

        if let Some(ref mut groups) = item.status_bar_group_menu {
            let flat_ids = remap_menu_codes_to_indices(groups);
            MENU_METADATA.lock().unwrap().flat_ids = flat_ids;
        }

        // Bridge call: ohos.statusbar/add — dispatched to the worker thread so the
        // main thread is never blocked waiting on a TSFN response (would deadlock).
        let request = StatusBarAddRequest::from(&item);
        dispatch_bridge_call(move || {
            with_statusbar_client("add", |client| {
                // Best-effort removal of any stale status-bar registration left by a
                // prior/killed instance of this app. statusBarManager rejects a
                // second addToStatusBar for the same ability with 16000078
                // "Multi-instance is not supported" (surfaced to the caller as a
                // generic 401 "check param error"). removeFromStatusBar is a no-op
                // when nothing is registered, so this is safe on a fresh launch.
                if let Err(e) =
                    futures_executor::block_on(client.remove(StatusBarRemoveRequest {}))
                {
                    log::warn!("[TrayIcon] pre-add remove (best-effort) error: {}", e);
                }
                if let Err(e) = futures_executor::block_on(client.add(request)) {
                    record_bridge_error("add", &e);
                }
            });
        });

        event::register_tray_id(id);
        event::start_event_forward_thread();
        TRAY_VISIBLE.store(true, Ordering::Release);

        Ok(Self {
            attrs: RefCell::new(attrs),
            is_visible: RefCell::new(true),
        })
    }

    pub fn set_icon(&mut self, icon: Option<crate::Icon>) -> crate::Result<()> {
        let is_template = self.attrs.borrow().icon_is_template;
        let request = if let Some(i) = &icon {
            let status_bar_icon = icon::icon_to_status_bar_icon(&i.inner, is_template)?;
            openharmony_ability_plugin_statusbar::StatusBarUpdateIconRequest::from(status_bar_icon)
        } else {
            // Clear icon by sending empty icon data
            let empty_icon = StatusBarIcon::default();
            openharmony_ability_plugin_statusbar::StatusBarUpdateIconRequest::from(empty_icon)
        };
        dispatch_bridge_call(move || {
            with_statusbar_client("update_icon", |client| {
                if let Err(e) = futures_executor::block_on(client.update_icon(request)) {
                    record_bridge_error("update_icon", &e);
                }
            });
        });
        self.attrs.borrow_mut().icon = icon;
        Ok(())
    }

    pub fn set_menu(&mut self, menu: Option<Box<dyn crate::menu::ContextMenu>>) {
        // See TrayIcon::new: muda owns the single check-state source.
        if let Some(m) = menu.as_ref().and_then(|m| m.as_menu()) {
            muda::register_check_items(m);
        }

        let (menus, predefined_map, about_metadata, menu_json) =
            menu_to_status_bar_items_with_metadata(&menu);

        {
            let mut metadata = MENU_METADATA.lock().unwrap();
            metadata.predefined_map = predefined_map;
            metadata.about_metadata = about_metadata;
            metadata.menu_json = menu_json;
        }

        // Build the request on the calling thread (consumes `menus`), then dispatch.
        let request = if let Some(mut m) = menus {
            let flat_ids = remap_menu_codes_to_indices(&mut m);
            MENU_METADATA.lock().unwrap().flat_ids = flat_ids;
            Some(openharmony_ability_plugin_statusbar::StatusBarUpdateMenuRequest::from(&m))
        } else if menu.is_none() {
            Some(openharmony_ability_plugin_statusbar::StatusBarUpdateMenuRequest::from(&vec![]))
        } else {
            None
        };
        if let Some(request) = request {
            dispatch_bridge_call(move || {
                with_statusbar_client("update_menu", |client| {
                    if let Err(e) = futures_executor::block_on(client.update_menu(request)) {
                        record_bridge_error("update_menu", &e);
                    }
                });
            });
        }
        self.attrs.borrow_mut().menu = menu;
    }

    pub fn set_tooltip<S: AsRef<str>>(&mut self, tooltip: Option<S>) -> crate::Result<()> {
        let tips = tooltip.and_then(|s| {
            let s = s.as_ref().to_string();
            if s.is_empty() { None } else { Some(s) }
        });
        if let Some(ref t) = tips {
            if t.len() > 128 {
                return Err(crate::Error::OhosError(
                    "tooltip exceeds 128 chars".to_string(),
                ));
            }
            let request = openharmony_ability_plugin_statusbar::StatusBarUpdateTipsRequest {
                tips: t.clone(),
            };
            dispatch_bridge_call(move || {
                with_statusbar_client("update_tips", |client| {
                    if let Err(e) = futures_executor::block_on(client.update_tips(request)) {
                        record_bridge_error("update_tips", &e);
                    }
                });
            });
        }
        self.attrs.borrow_mut().tooltip = tips;
        Ok(())
    }

    pub fn set_title<S: AsRef<str>>(&mut self, title: Option<S>) {
        if let Some(t) = title {
            self.attrs.borrow_mut().title = Some(t.as_ref().to_string());
        } else {
            self.attrs.borrow_mut().title = None;
        }
        if *self.is_visible.borrow() {
            // Pre-compute the add request on the calling thread (borrows self.attrs),
            // then dispatch remove+add together in one closure so they stay ordered.
            let item = build_item_from_attrs(&self.attrs.borrow()).ok();
            let add_request = item.as_ref().map(|i| StatusBarAddRequest::from(i));
            dispatch_bridge_call(move || {
                with_statusbar_client("set_title", |client| {
                    if let Err(e) =
                        futures_executor::block_on(client.remove(StatusBarRemoveRequest {}))
                    {
                        record_bridge_error("set_title remove", &e);
                    }
                    if let Some(request) = add_request {
                        if let Err(e) = futures_executor::block_on(client.add(request)) {
                            record_bridge_error("set_title add", &e);
                        }
                    }
                });
            });
        }
    }

    pub fn set_visible(&mut self, visible: bool) -> crate::Result<()> {
        if visible && !*self.is_visible.borrow() {
            let item = build_item_from_attrs(&self.attrs.borrow())?;
            let request = StatusBarAddRequest::from(&item);
            dispatch_bridge_call(move || {
                with_statusbar_client("set_visible", |client| {
                    if let Err(e) = futures_executor::block_on(client.add(request)) {
                        record_bridge_error("set_visible add", &e);
                    }
                });
            });
            *self.is_visible.borrow_mut() = true;
            TRAY_VISIBLE.store(true, Ordering::Release);
        } else if !visible && *self.is_visible.borrow() {
            dispatch_bridge_call(|| {
                with_statusbar_client("set_visible", |client| {
                    if let Err(e) =
                        futures_executor::block_on(client.remove(StatusBarRemoveRequest {}))
                    {
                        record_bridge_error("set_visible remove", &e);
                    }
                });
            });
            *self.is_visible.borrow_mut() = false;
            TRAY_VISIBLE.store(false, Ordering::Release);
        }
        Ok(())
    }

    pub fn set_quick_operation(&mut self, config: Option<crate::QuickOperationConfig>) {
        self.attrs.borrow_mut().quick_operation = config;
        if *self.is_visible.borrow() {
            let item = build_item_from_attrs(&self.attrs.borrow()).ok();
            let add_request = item.as_ref().map(|i| StatusBarAddRequest::from(i));
            dispatch_bridge_call(move || {
                with_statusbar_client("set_quick_operation", |client| {
                    if let Err(e) =
                        futures_executor::block_on(client.remove(StatusBarRemoveRequest {}))
                    {
                        record_bridge_error("set_quick_operation remove", &e);
                    }
                    if let Some(request) = add_request {
                        if let Err(e) = futures_executor::block_on(client.add(request)) {
                            record_bridge_error("set_quick_operation add", &e);
                        }
                    }
                });
            });
        }
    }

    pub fn set_temp_dir_path<P: AsRef<std::path::Path>>(&mut self, _path: Option<P>) {}

    pub fn set_icon_as_template(&mut self, is_template: bool) -> crate::Result<()> {
        // No-op if value unchanged — avoids unnecessary remove+re-add
        if self.attrs.borrow().icon_is_template == is_template {
            return Ok(());
        }
        self.attrs.borrow_mut().icon_is_template = is_template;
        if *self.is_visible.borrow() {
            let item = build_item_from_attrs(&self.attrs.borrow())?;
            let request = StatusBarAddRequest::from(&item);
            dispatch_bridge_call(move || {
                with_statusbar_client("set_icon_as_template", |client| {
                    if let Err(e) =
                        futures_executor::block_on(client.remove(StatusBarRemoveRequest {}))
                    {
                        record_bridge_error("set_icon_as_template remove", &e);
                    }
                    if let Err(e) = futures_executor::block_on(client.add(request)) {
                        record_bridge_error("set_icon_as_template add", &e);
                    }
                });
            });
        }
        Ok(())
    }

    pub fn set_icon_with_as_template(
        &mut self,
        icon: Option<crate::Icon>,
        is_template: bool,
    ) -> crate::Result<()> {
        self.attrs.borrow_mut().icon_is_template = is_template;
        self.set_icon(icon)
    }

    // OHOS: rect() always returns None.
    // StatusBar API does not provide tray icon position or dimensions.
    // AvoidArea.topRect returns the entire status bar area (e.g. {0,0,1440,48}),
    // not the tray icon itself — using it as an approximation would mislead callers
    // who rely on rect for popup positioning or size calculations.
    // This is consistent with Linux, which also returns None.
    pub fn rect(&self) -> Option<crate::Rect> {
        None
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        if *self.is_visible.borrow() {
            // Closure captures no `self` — only re-fetches the 'static client on the worker.
            dispatch_bridge_call(|| {
                with_statusbar_client("remove", |client| {
                    if let Err(e) =
                        futures_executor::block_on(client.remove(StatusBarRemoveRequest {}))
                    {
                        record_bridge_error("remove on drop", &e);
                    }
                    // No unregister_*_handler calls — plugin lifecycle manages event registration.
                });
            });
            TRAY_VISIBLE.store(false, Ordering::Release);
        }
    }
}

fn menu_to_status_bar_items(
    menu: &Option<Box<dyn crate::menu::ContextMenu>>,
) -> Option<Vec<Vec<StatusBarMenuItem>>> {
    menu.as_ref().and_then(|m| {
        let json = m.ohos_context_menu();
        let items: Vec<MenuJsonItem> = match serde_json::from_str::<Vec<MenuJsonItem>>(&json) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("[TrayIcon] menu_to_status_bar_items: serde deserialized FAILED: {}", e);
                Vec::new()
            }
        };
        if items.is_empty() {
            None
        } else {
            let groups = split_items_into_groups(items);
            if groups.is_empty() {
                None
            } else {
                Some(groups)
            }
        }
    })
}

/// Extract menu metadata (predefined_map, about_metadata, menu_json) without
/// converting to StatusBarMenuItem. Used by `new()` which delegates the
/// conversion to `build_item_from_attrs()`.
fn extract_menu_metadata(
    menu: &Option<Box<dyn crate::menu::ContextMenu>>,
) -> (
    HashMap<String, String>,
    HashMap<String, AboutMetadataJson>,
    Option<String>,
) {
    let Some(m) = menu.as_ref() else {
        return (HashMap::new(), HashMap::new(), None);
    };
    let json = m.ohos_context_menu();
    let items: Vec<MenuJsonItem> = match serde_json::from_str::<Vec<MenuJsonItem>>(&json) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("[TrayIcon] extract_menu_metadata: serde deserialized FAILED: {}", e);
            Vec::new()
        }
    };
    if items.is_empty() {
        return (HashMap::new(), HashMap::new(), None);
    }

    let mut predefined_map = HashMap::new();
    let mut about_metadata = HashMap::new();
    collect_metadata_from_items(&items, &mut predefined_map, &mut about_metadata);

    (predefined_map, about_metadata, Some(json))
}

fn menu_to_status_bar_items_with_metadata(
    menu: &Option<Box<dyn crate::menu::ContextMenu>>,
) -> (
    Option<Vec<Vec<StatusBarMenuItem>>>,
    HashMap<String, String>,
    HashMap<String, AboutMetadataJson>,
    Option<String>,
) {
    let empty = (None, HashMap::new(), HashMap::new(), None);
    let Some(m) = menu.as_ref() else {
        return empty;
    };
    let json = m.ohos_context_menu();
    let items: Vec<MenuJsonItem> = serde_json::from_str(&json).unwrap_or_default();
    if items.is_empty() {
        return empty;
    }

    let mut predefined_map = HashMap::new();
    let mut about_metadata = HashMap::new();

    collect_metadata_from_items(&items, &mut predefined_map, &mut about_metadata);

    let groups = split_items_into_groups(items);

    let result = if groups.is_empty() {
        None
    } else {
        Some(groups)
    };

    (result, predefined_map, about_metadata, Some(json))
}

// OHOS status-bar menu items cannot be disabled (StatusBarMenuItem has no
// enabled flag), so muda's disabled items stay clickable. Warn once per
// process so developers know that state is not honored.
static WARNED_DISABLED_ITEMS: AtomicBool = AtomicBool::new(false);

fn warn_disabled_item_once() {
    if !WARNED_DISABLED_ITEMS.swap(true, Ordering::Relaxed) {
        log::warn!(
            "[TrayIcon] menu contains disabled items, but OHOS status-bar items cannot be disabled — they will remain clickable"
        );
    }
}

fn collect_metadata_from_items(
    items: &[MenuJsonItem],
    predefined_map: &mut HashMap<String, String>,
    about_metadata: &mut HashMap<String, AboutMetadataJson>,
) {
    for item in items {
        if item.item_type == "predefined" {
            if let Some(ref pt) = item.predefined_type {
                if pt != "separator" {
                    predefined_map.insert(item.id.clone(), pt.clone());
                }
                if pt == "about" {
                    if let Some(ref meta) = item.about_metadata {
                        about_metadata.insert(item.id.clone(), meta.clone());
                    }
                }
            }
        }
        if item.enabled == Some(false) && !is_separator(item) {
            warn_disabled_item_once();
        }
        if let Some(ref children) = item.submenu_items {
            collect_metadata_from_items(children, predefined_map, about_metadata);
        }
    }
}

pub(crate) fn split_items_into_groups(
    items: Vec<MenuJsonItem>,
) -> Vec<Vec<StatusBarMenuItem>> {
    let mut groups: Vec<Vec<StatusBarMenuItem>> = Vec::new();
    let mut current_group: Vec<StatusBarMenuItem> = Vec::new();

    for item in items {
        if is_separator(&item) {
            if !current_group.is_empty() {
                groups.push(current_group);
                current_group = Vec::new();
            }
        } else {
            current_group.push(menu_json_item_to_status_bar_item(item));
        }
    }
    if !current_group.is_empty() {
        groups.push(current_group);
    }

    groups
}

/// Replace all menuCode values with sequential numeric strings ("0", "1", ...).
/// Returns the flat_ids mapping: index → our original string ID.
pub(crate) fn remap_menu_codes_to_indices(
    groups: &mut [Vec<StatusBarMenuItem>],
) -> Vec<String> {
    let mut flat_ids = Vec::new();
    let mut idx: usize = 0;
    for group in groups.iter_mut() {
        for item in group.iter_mut() {
            if let Some(ref code) = item.menu_code {
                flat_ids.push(code.clone());
                let num = idx.to_string();
                item.menu_code = Some(num.clone());
                if let Some(ref mut action) = item.menu_action {
                    action.menu_code = Some(num);
                }
                idx += 1;
            }
            if let Some(ref mut sub_menu) = item.sub_menu {
                for sub in sub_menu.iter_mut() {
                    if let Some(ref code) = sub.menu_code {
                        flat_ids.push(code.clone());
                        let num = idx.to_string();
                        sub.menu_code = Some(num.clone());
                        sub.menu_action.menu_code = Some(num);
                        idx += 1;
                    }
                }
            }
        }
    }
    flat_ids
}

/// Strips Windows-style mnemonics: a single `&` marks the next character as
/// the mnemonic (dropped), while `&&` is an escaped literal `&` (kept, single).
/// This is the single stripping point for status-bar menu titles — muda's
/// serializer keeps the raw text.
fn strip_mnemonics(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            if chars.peek() == Some(&'&') {
                chars.next();
                out.push('&');
            }
            // Single `&` marks a mnemonic — drop it.
        } else {
            out.push(c);
        }
    }
    out
}

fn decode_png_to_rgba(png_bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder.read_info().map_err(|e| format!("PNG read_info failed: {}", e))?;
    let buf_size = reader.output_buffer_size().ok_or("PNG output_buffer_size failed")?;
    let mut buf = vec![0u8; buf_size];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("PNG next_frame failed: {}", e))?;
    let width = info.width;
    let height = info.height;

    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => {
            let rgb = &buf[..info.buffer_size()];
            let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
            for chunk in rgb.chunks(3) {
                rgba.extend_from_slice(chunk);
                rgba.push(255);
            }
            rgba
        }
        _ => return Err(format!("Unsupported PNG color type: {:?}", info.color_type)),
    };

    Ok((rgba, width, height))
}

#[derive(serde::Deserialize, Clone)]
#[allow(dead_code)]
pub(crate) struct MenuJsonItem {
    id: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(rename = "type", default)]
    item_type: String,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    accelerator: Option<String>,
    #[serde(rename = "predefinedType")]
    predefined_type: Option<String>,
    #[serde(rename = "submenuItems")]
    submenu_items: Option<Vec<MenuJsonItem>>,
    #[serde(default)]
    checked: Option<bool>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(rename = "aboutMetadata", default)]
    pub(crate) about_metadata: Option<AboutMetadataJson>,
}

#[derive(serde::Deserialize, Clone, Default)]
#[allow(dead_code)]
pub(crate) struct AboutMetadataJson {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) version: Option<String>,
    #[serde(rename = "shortVersion", default)]
    pub(crate) short_version: Option<String>,
    #[serde(default)]
    pub(crate) authors: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) comments: Option<String>,
    #[serde(default)]
    pub(crate) copyright: Option<String>,
    #[serde(default)]
    pub(crate) license: Option<String>,
    #[serde(default)]
    pub(crate) website: Option<String>,
}

impl From<AboutMetadataJson> for openharmony_ability_plugin_statusbar::StatusBarAboutMetadata {
    fn from(meta: AboutMetadataJson) -> Self {
        use openharmony_ability_plugin_statusbar::StatusBarAboutMetadata;
        StatusBarAboutMetadata {
            name: meta.name,
            version: meta.version,
            short_version: meta.short_version,
            authors: meta.authors,
            comments: meta.comments,
            copyright: meta.copyright,
            license: meta.license,
            website: meta.website,
        }
    }
}

pub(crate) fn is_separator(item: &MenuJsonItem) -> bool {
    item.item_type == "separator"
        || (item.item_type == "predefined"
            && item.predefined_type.as_deref() == Some("separator"))
}

pub(crate) fn menu_json_item_to_status_bar_item(
    item: MenuJsonItem,
) -> StatusBarMenuItem {
    if item.item_type == "submenu" {
        let sub_items: Vec<StatusBarSubMenuItem> = item
            .submenu_items
            .unwrap_or_default()
            .into_iter()
            .filter(|child| !is_separator(child))
            .map(|child| {
                let text = strip_mnemonics(&child.text.unwrap_or_default());
                let options = build_item_options(&child.item_type, child.checked, child.icon.as_deref());
                StatusBarSubMenuItem {
                    sub_title: text,
                    menu_code: Some(child.id.clone()),
                    menu_action: StatusBarMenuAction {
                        ability_name: String::new(),
                        module_name: None,
                        menu_code: Some(child.id),
                        notify_only: Some(true),
                    },
                    options,
                }
            })
            .collect();

        StatusBarMenuItem {
            title: strip_mnemonics(&item.text.unwrap_or_default()),
            menu_code: None,
            sub_menu: Some(sub_items),
            menu_action: None,
            options: None,
        }
    } else {
        let options = build_item_options(&item.item_type, item.checked, item.icon.as_deref());
        StatusBarMenuItem {
            title: strip_mnemonics(&item.text.unwrap_or_default()),
            menu_code: Some(item.id.clone()),
            sub_menu: None,
            menu_action: Some(StatusBarMenuAction {
                ability_name: String::new(),
                module_name: None,
                menu_code: Some(item.id),
                notify_only: Some(true),
            }),
            options,
        }
    }
}

fn build_item_options(
    item_type: &str,
    checked: Option<bool>,
    icon_b64: Option<&str>,
) -> Option<StatusBarMenuItemOptions> {
    let selected = if item_type == "check" { checked } else { None };

    let (icon_rgba, icon_width, icon_height) = if item_type == "icon" {
        decode_icon_from_base64(icon_b64)
    } else {
        (None, None, None)
    };

    if selected.is_some() || icon_rgba.is_some() {
        Some(StatusBarMenuItemOptions {
            icon: None,
            selected,
            selected_icon: None,
            icon_rgba,
            icon_width,
            icon_height,
        })
    } else {
        None
    }
}

fn decode_icon_from_base64(icon_b64: Option<&str>) -> (Option<Vec<u8>>, Option<u32>, Option<u32>) {
    let Some(b64) = icon_b64 else {
        return (None, None, None);
    };
    use base64::Engine;
    let Ok(png_bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else {
        return (None, None, None);
    };
    match decode_png_to_rgba(&png_bytes) {
        Ok((rgba, width, height)) => {
            (Some(rgba), Some(width), Some(height))
        }
        Err(e) => {
            log::warn!("[TrayIcon] decode_icon_from_base64: PNG decode failed: {}", e);
            (None, None, None)
        }
    }
}

fn build_item_from_attrs(
    attrs: &TrayIconAttributes,
) -> crate::Result<StatusBarItem> {
    let icon = attrs.icon.as_ref().ok_or_else(|| {
        crate::Error::OsError(io::Error::new(
            io::ErrorKind::InvalidData,
            "No icon provided",
        ))
    })?;

    let status_bar_icon = icon::icon_to_status_bar_icon(&icon.inner, attrs.icon_is_template)?;

    let quick_operation = if let Some(ref config) = attrs.quick_operation {
        QuickOperation {
            ability_name: config.ability_name.clone(),
            // Empty title → the ArkTS side falls back to the app name.
            title: if config.title.is_empty() {
                attrs.title.clone().unwrap_or_default()
            } else {
                config.title.clone()
            },
            height: config.height,
            module_name: config.module_name.clone(),
            loading_status: config.loading_status,
        }
    } else {
        // No embedder configuration: stay neutral — an empty title falls back
        // to the app name on the ArkTS side and `None` uses the system-default
        // module. Tauri-app defaults (product name / "entry" module) belong to
        // the tauri crate's tray builder, not this platform-agnostic library.
        QuickOperation {
            ability_name: String::new(),
            title: attrs.title.clone().unwrap_or_default(),
            height: 200,
            module_name: None,
            loading_status: None,
        }
    };

    let menus = menu_to_status_bar_items(&attrs.menu);

    Ok(StatusBarItem {
        icons: status_bar_icon,
        quick_operation,
        status_bar_group_menu: menus,
        hover_tips: attrs.tooltip.clone().filter(|s| !s.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn make_item(id: &str, text: &str) -> MenuJsonItem {
        MenuJsonItem {
            id: id.to_string(),
            text: Some(text.to_string()),
            enabled: Some(true),
            accelerator: None,
            item_type: "item".to_string(),
            predefined_type: None,
            submenu_items: None,
            checked: None,
            icon: None,
            about_metadata: None,
        }
    }

    fn make_separator(id: &str) -> MenuJsonItem {
        MenuJsonItem {
            id: id.to_string(),
            text: None,
            enabled: Some(false),
            accelerator: None,
            item_type: "predefined".to_string(),
            predefined_type: Some("separator".to_string()),
            submenu_items: None,
            checked: None,
            icon: None,
            about_metadata: None,
        }
    }

    fn make_submenu(id: &str, text: &str, children: Vec<MenuJsonItem>) -> MenuJsonItem {
        MenuJsonItem {
            id: id.to_string(),
            text: Some(text.to_string()),
            enabled: Some(true),
            accelerator: None,
            item_type: "submenu".to_string(),
            predefined_type: None,
            submenu_items: Some(children),
            checked: None,
            icon: None,
            about_metadata: None,
        }
    }

    fn make_status_bar_item(code: &str) -> StatusBarMenuItem {
        StatusBarMenuItem {
            title: code.to_string(),
            menu_code: Some(code.to_string()),
            sub_menu: None,
            menu_action: Some(StatusBarMenuAction {
                ability_name: String::new(),
                module_name: None,
                menu_code: Some(code.to_string()),
                notify_only: Some(true),
            }),
            options: None,
        }
    }

    #[test]
    fn test_regular_item_becomes_top_level_menu_item() {
        let item = make_item("item_1", "Open");
        let result = menu_json_item_to_status_bar_item(item);

        assert_eq!(result.title, "Open");
        assert!(result.sub_menu.is_none());
        assert!(result.menu_action.is_some());
        let action = result.menu_action.unwrap();
        assert_eq!(action.menu_code, Some("item_1".to_string()));
        assert!(action.notify_only.unwrap());
    }

    #[test]
    fn test_submenu_becomes_item_with_sub_menu() {
        let item = make_submenu("submenu_1", "File", vec![
            make_item("item_new", "New"),
            make_item("item_open", "Open"),
        ]);
        let result = menu_json_item_to_status_bar_item(item);

        assert_eq!(result.title, "File");
        assert!(result.menu_action.is_none());
        assert!(result.sub_menu.is_some());
        let sub_items = result.sub_menu.unwrap();
        assert_eq!(sub_items.len(), 2);
        assert_eq!(sub_items[0].sub_title, "New");
        assert_eq!(sub_items[0].menu_code, Some("item_new".to_string()));
        assert_eq!(sub_items[1].sub_title, "Open");
        assert_eq!(sub_items[1].menu_code, Some("item_open".to_string()));
    }

    #[test]
    fn test_separators_filtered_out() {
        let items = vec![
            make_item("item_1", "Copy"),
            make_separator("sep_1"),
            make_item("item_2", "Paste"),
        ];
        let menu_items: Vec<_> = items
            .into_iter()
            .filter(|item| !is_separator(item))
            .map(|item| menu_json_item_to_status_bar_item(item))
            .collect();

        assert_eq!(menu_items.len(), 2);
        assert_eq!(menu_items[0].title, "Copy");
        assert_eq!(menu_items[1].title, "Paste");
    }

    #[test]
    fn test_menu_json_deserialization_from_muda_format() {
        let json = r#"[
            {"id":"1","type":"item","text":"Open","enabled":true},
            {"id":"2","type":"predefined","text":"","enabled":true,"predefinedType":"separator"},
            {"id":"3","type":"submenu","text":"File","enabled":true,"submenuItems":[
                {"id":"3a","type":"item","text":"New","enabled":true}
            ]}
        ]"#;
        let items: Vec<MenuJsonItem> = serde_json::from_str(json).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].item_type, "item");
        assert_eq!(items[0].text, Some("Open".to_string()));
        assert_eq!(items[1].item_type, "predefined");
        assert_eq!(items[1].predefined_type, Some("separator".to_string()));
        assert_eq!(items[2].item_type, "submenu");
        assert_eq!(items[2].submenu_items.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_split_items_into_groups_at_separator() {
        let items = vec![
            make_item("item_1", "Copy"),
            make_separator("sep_1"),
            make_item("item_2", "Paste"),
        ];
        let groups = split_items_into_groups(items);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 1);
        assert_eq!(groups[0][0].title, "Copy");
        assert_eq!(groups[1].len(), 1);
        assert_eq!(groups[1][0].title, "Paste");
    }

    #[test]
    fn build_item_with_quick_operation() {
        use crate::QuickOperationConfig;
        let mut attrs = TrayIconAttributes::default();
        attrs.quick_operation = Some(QuickOperationConfig {
            title: "My Panel".into(),
            height: 300,
            ability_name: "MyAbility".into(),
            module_name: Some("entry".into()),
            loading_status: Some(true),
        });

        // Verify the QuickOperation struct that build_item_from_attrs would create
        let config = attrs.quick_operation.as_ref().unwrap();
        let qo = QuickOperation {
            ability_name: config.ability_name.clone(),
            title: if config.title.is_empty() {
                attrs.title.clone().unwrap_or_default()
            } else {
                config.title.clone()
            },
            height: config.height,
            module_name: config.module_name.clone(),
            loading_status: config.loading_status,
        };

        assert_eq!(qo.ability_name, "MyAbility");
        assert_eq!(qo.title, "My Panel");
        assert_eq!(qo.height, 300);
        assert_eq!(qo.module_name, Some("entry".into()));
        assert_eq!(qo.loading_status, Some(true));
    }

    #[test]
    fn build_item_without_quick_operation() {
        let mut attrs = TrayIconAttributes::default();
        attrs.title = Some("My App".into());
        // quick_operation is None by default

        // Verify the neutral default QuickOperation struct that
        // build_item_from_attrs would create: the title falls back to attrs and
        // the module uses the system default — Tauri-app defaults (product
        // name / "entry") live in the tauri crate's tray builder, not here.
        let qo = QuickOperation {
            ability_name: String::new(),
            title: attrs.title.clone().unwrap_or_default(),
            height: 200,
            module_name: None,
            loading_status: None,
        };

        assert!(qo.ability_name.is_empty());
        assert_eq!(qo.title, "My App");
        assert_eq!(qo.height, 200);
        assert_eq!(qo.module_name, None);
        assert!(qo.loading_status.is_none());
    }

    #[test]
    fn quick_operation_empty_title_falls_back_to_attrs_title() {
        use crate::QuickOperationConfig;
        let mut attrs = TrayIconAttributes::default();
        attrs.title = Some("App Title".into());
        attrs.quick_operation = Some(QuickOperationConfig {
            title: String::new(), // empty → should fall back to attrs.title
            height: 250,
            ability_name: "TestAbility".into(),
            module_name: None,
            loading_status: None,
        });

        let config = attrs.quick_operation.as_ref().unwrap();
        let title = if config.title.is_empty() {
            attrs.title.clone().unwrap_or_default()
        } else {
            config.title.clone()
        };

        assert_eq!(title, "App Title");
    }

    #[test]
    fn quick_operation_no_titles_is_empty_for_arkts_fallback() {
        use crate::QuickOperationConfig;
        let mut attrs = TrayIconAttributes::default();
        // attrs.title is None
        attrs.quick_operation = Some(QuickOperationConfig {
            title: String::new(), // empty
            height: 250,
            ability_name: "TestAbility".into(),
            module_name: None,
            loading_status: None,
        });

        let config = attrs.quick_operation.as_ref().unwrap();
        let title = if config.title.is_empty() {
            attrs.title.clone().unwrap_or_default()
        } else {
            config.title.clone()
        };

        // Neutral default: the ArkTS side falls back to the app name
        assert_eq!(title, "");
    }

    #[test]
    fn test_icon_is_template_default_false() {
        let attrs = TrayIconAttributes::default();
        assert_eq!(attrs.icon_is_template, false);
    }

    #[test]
    fn test_build_item_from_attrs_template_mode() {
        use crate::icon::Icon;

        // Create a simple 2x2 red icon
        let rgba = vec![255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 128, 128, 128, 128];
        let icon = Icon::from_rgba(rgba, 2, 2).unwrap();

        let mut attrs = TrayIconAttributes::default();
        attrs.icon = Some(icon);
        attrs.icon_is_template = true;

        let item = build_item_from_attrs(&attrs).unwrap();
        let white = item.icons.white.borrow().clone().unwrap();
        let black = item.icons.black.borrow().clone().unwrap();

        // Template mode: white and black should differ
        assert_ne!(white, black);
        // White version: RGB=255
        assert_eq!(white[0], 255);
        assert_eq!(white[1], 255);
        assert_eq!(white[2], 255);
        // Black version: RGB=0
        assert_eq!(black[0], 0);
        assert_eq!(black[1], 0);
        assert_eq!(black[2], 0);
    }

    #[test]
    fn test_build_item_from_attrs_not_template() {
        use crate::icon::Icon;

        let rgba = vec![255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 128, 128, 128, 128];
        let icon = Icon::from_rgba(rgba.clone(), 2, 2).unwrap();

        let mut attrs = TrayIconAttributes::default();
        attrs.icon = Some(icon);
        attrs.icon_is_template = false;

        let item = build_item_from_attrs(&attrs).unwrap();
        let white = item.icons.white.borrow().clone().unwrap();
        let black = item.icons.black.borrow().clone().unwrap();

        // Not template: white and black should be the same original image
        assert_eq!(white, black);
        assert_eq!(white, rgba);
    }

    // ─── remap_menu_codes_to_indices ──────────────────────────────────────

    #[test]
    fn test_remap_menu_codes_single_group() {
        let mut groups: Vec<Vec<StatusBarMenuItem>> = vec![vec![
            make_status_bar_item("a"),
            make_status_bar_item("b"),
            make_status_bar_item("c"),
        ]];
        let flat_ids = remap_menu_codes_to_indices(&mut groups);
        assert_eq!(flat_ids, vec!["a", "b", "c"]);
        assert_eq!(groups[0][0].menu_code, Some("0".to_string()));
        assert_eq!(groups[0][1].menu_code, Some("1".to_string()));
        assert_eq!(groups[0][2].menu_code, Some("2".to_string()));
        // menu_action should also be remapped
        assert_eq!(groups[0][0].menu_action.as_ref().unwrap().menu_code, Some("0".to_string()));
    }

    #[test]
    fn test_remap_menu_codes_multiple_groups() {
        let mut groups: Vec<Vec<StatusBarMenuItem>> = vec![
            vec![make_status_bar_item("a"), make_status_bar_item("b")],
            vec![make_status_bar_item("c")],
        ];
        let flat_ids = remap_menu_codes_to_indices(&mut groups);
        assert_eq!(flat_ids, vec!["a", "b", "c"]);
        assert_eq!(groups[0][0].menu_code, Some("0".to_string()));
        assert_eq!(groups[0][1].menu_code, Some("1".to_string()));
        assert_eq!(groups[1][0].menu_code, Some("2".to_string()));
    }

    #[test]
    fn test_remap_menu_codes_with_submenus() {
        let sub = StatusBarSubMenuItem {
            sub_title: "Sub".to_string(),
            menu_code: Some("sub_id".to_string()),
            menu_action: StatusBarMenuAction {
                ability_name: String::new(),
                module_name: None,
                menu_code: Some("sub_id".to_string()),
                notify_only: Some(true),
            },
            options: None,
        };
        let mut groups: Vec<Vec<StatusBarMenuItem>> = vec![vec![
            StatusBarMenuItem {
                title: "Parent".to_string(),
                menu_code: Some("parent_id".to_string()),
                sub_menu: Some(vec![sub]),
                menu_action: Some(StatusBarMenuAction {
                    ability_name: String::new(),
                    module_name: None,
                    menu_code: Some("parent_id".to_string()),
                    notify_only: Some(true),
                }),
                options: None,
            },
        ]];
        let flat_ids = remap_menu_codes_to_indices(&mut groups);
        assert_eq!(flat_ids, vec!["parent_id", "sub_id"]);
        assert_eq!(groups[0][0].menu_code, Some("0".to_string()));
        assert_eq!(groups[0][0].sub_menu.as_ref().unwrap()[0].menu_code, Some("1".to_string()));
    }

    #[test]
    fn test_remap_menu_codes_empty_groups() {
        let mut groups: Vec<Vec<StatusBarMenuItem>> = vec![];
        let flat_ids = remap_menu_codes_to_indices(&mut groups);
        assert!(flat_ids.is_empty());
    }

    // ─── strip_mnemonics ──────────────────────────────────────────────────

    #[test]
    fn test_strip_mnemonics_single_ampersand() {
        assert_eq!(strip_mnemonics("Save &As"), "Save As");
        assert_eq!(strip_mnemonics("&File"), "File");
        assert_eq!(strip_mnemonics("F&ormat"), "Format");
    }

    #[test]
    fn test_strip_mnemonics_double_ampersand() {
        // Double ampersand is an escaped literal & — strips to a single &
        assert_eq!(strip_mnemonics("A&&B"), "A&B");
        assert_eq!(strip_mnemonics("&&&&"), "&&");
    }

    #[test]
    fn test_strip_mnemonics_no_ampersand() {
        assert_eq!(strip_mnemonics("Plain Text"), "Plain Text");
        assert_eq!(strip_mnemonics(""), "");
    }

    // ─── decode_icon_from_base64 ─────────────────────────────────────────

    #[test]
    fn test_decode_icon_from_base64_none() {
        let (rgba, w, h) = decode_icon_from_base64(None);
        assert!(rgba.is_none());
        assert!(w.is_none());
        assert!(h.is_none());
    }

    #[test]
    fn test_decode_icon_from_base64_invalid_base64() {
        let (rgba, w, h) = decode_icon_from_base64(Some("!!!not_base64!!!"));
        assert!(rgba.is_none());
        assert!(w.is_none());
        assert!(h.is_none());
    }

    #[test]
    fn test_decode_icon_from_base64_invalid_png() {
        // Valid base64 but not a valid PNG
        let bad = base64::engine::general_purpose::STANDARD.encode(b"not a png");
        let (rgba, w, h) = decode_icon_from_base64(Some(&bad));
        assert!(rgba.is_none());
        assert!(w.is_none());
        assert!(h.is_none());
    }

    #[test]
    fn test_decode_icon_from_base64_valid_png() {
        // Create a minimal 1x1 RGBA PNG
        let rgba = vec![255, 0, 0, 255];
        let mut png_data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_data, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&rgba).unwrap();
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png_data);
        let (decoded_rgba, w, h) = decode_icon_from_base64(Some(&b64));
        assert_eq!(w, Some(1));
        assert_eq!(h, Some(1));
        assert!(decoded_rgba.is_some());
        assert_eq!(decoded_rgba.unwrap(), rgba);
    }

    // ─── decode_png_to_rgba ───────────────────────────────────────────────

    #[test]
    fn test_decode_png_to_rgba_rgb_to_rgba_conversion() {
        // Create a 2x1 RGB (no alpha) PNG and verify alpha=255 is added
        let rgb = vec![255, 0, 0, 0, 255, 0];
        let mut png_data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_data, 2, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&rgb).unwrap();
        }
        let (rgba, w, h) = decode_png_to_rgba(&png_data).unwrap();
        assert_eq!(w, 2);
        assert_eq!(h, 1);
        assert_eq!(rgba.len(), 8);
        assert_eq!(rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn test_decode_png_to_rgba_invalid_data() {
        let result = decode_png_to_rgba(b"not a png");
        assert!(result.is_err());
    }

    // ─── build_item_options ──────────────────────────────────────────────

    #[test]
    fn test_build_item_options_check_selected_true() {
        let opts = build_item_options("check", Some(true), None);
        assert!(opts.is_some());
        assert_eq!(opts.unwrap().selected, Some(true));
    }

    #[test]
    fn test_build_item_options_check_selected_false() {
        let opts = build_item_options("check", Some(false), None);
        assert!(opts.is_some());
        assert_eq!(opts.unwrap().selected, Some(false));
    }

    #[test]
    fn test_build_item_options_non_check_returns_none() {
        // Non-check items without icon should return None
        let opts = build_item_options("item", Some(true), None);
        assert!(opts.is_none());
    }

    #[test]
    fn test_build_item_options_icon_without_valid_base64() {
        // Icon type but no base64 data → None
        let opts = build_item_options("icon", None, None);
        assert!(opts.is_none());
    }

    // ─── collect_metadata_from_items ─────────────────────────────────────

    #[test]
    fn test_collect_metadata_predefined_items() {
        let items = vec![
            MenuJsonItem {
                id: "p1".to_string(),
                text: Some("Copy".to_string()),
                item_type: "predefined".to_string(),
                enabled: Some(true),
                accelerator: None,
                predefined_type: Some("copy".to_string()),
                submenu_items: None,
                checked: None,
                icon: None,
                about_metadata: None,
            },
            MenuJsonItem {
                id: "sep1".to_string(),
                text: None,
                item_type: "predefined".to_string(),
                enabled: Some(false),
                accelerator: None,
                predefined_type: Some("separator".to_string()),
                submenu_items: None,
                checked: None,
                icon: None,
                about_metadata: None,
            },
        ];
        let mut predefined_map = HashMap::new();
        let mut about_metadata = HashMap::new();
        collect_metadata_from_items(&items, &mut predefined_map, &mut about_metadata);
        // "copy" should be in the map, "separator" should be excluded
        assert_eq!(predefined_map.len(), 1);
        assert_eq!(predefined_map.get("p1"), Some(&"copy".to_string()));
        assert!(!predefined_map.contains_key("sep1"));
        assert!(about_metadata.is_empty());
    }

    #[test]
    fn test_collect_metadata_about_metadata() {
        let items = vec![MenuJsonItem {
            id: "about1".to_string(),
            text: Some("About".to_string()),
            item_type: "predefined".to_string(),
            enabled: Some(true),
            accelerator: None,
            predefined_type: Some("about".to_string()),
            submenu_items: None,
            checked: None,
            icon: None,
            about_metadata: Some(AboutMetadataJson {
                name: Some("TestApp".to_string()),
                version: Some("1.0.0".to_string()),
                ..Default::default()
            }),
        }];
        let mut predefined_map = HashMap::new();
        let mut about_metadata = HashMap::new();
        collect_metadata_from_items(&items, &mut predefined_map, &mut about_metadata);
        assert_eq!(predefined_map.get("about1"), Some(&"about".to_string()));
        let meta = about_metadata.get("about1").unwrap();
        assert_eq!(meta.name.as_deref(), Some("TestApp"));
        assert_eq!(meta.version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn test_collect_metadata_recursive_into_submenus() {
        let items = vec![
            MenuJsonItem {
                id: "top".to_string(),
                text: Some("File".to_string()),
                item_type: "submenu".to_string(),
                enabled: Some(true),
                accelerator: None,
                predefined_type: None,
                submenu_items: Some(vec![
                    MenuJsonItem {
                        id: "nested_check".to_string(),
                        text: Some("Nested".to_string()),
                        item_type: "check".to_string(),
                        enabled: Some(true),
                        accelerator: None,
                        predefined_type: None,
                        submenu_items: None,
                        checked: Some(true),
                        icon: None,
                        about_metadata: None,
                    },
                ]),
                checked: None,
                icon: None,
                about_metadata: None,
            },
        ];
        let mut predefined_map = HashMap::new();
        let mut about_metadata = HashMap::new();
        collect_metadata_from_items(&items, &mut predefined_map, &mut about_metadata);
        assert!(!predefined_map.contains_key("nested_check"));
    }

    // ─── split_items_into_groups edge cases ──────────────────────────────

    #[test]
    fn test_split_groups_leading_separator() {
        let items = vec![
            make_separator("sep_1"),
            make_item("item_1", "Copy"),
        ];
        let groups = split_items_into_groups(items);
        // Leading separator is ignored (current_group is empty)
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 1);
        assert_eq!(groups[0][0].title, "Copy");
    }

    #[test]
    fn test_split_groups_trailing_separator() {
        let items = vec![
            make_item("item_1", "Copy"),
            make_separator("sep_1"),
        ];
        let groups = split_items_into_groups(items);
        // Trailing separator just ends the last group
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 1);
    }

    #[test]
    fn test_split_groups_consecutive_separators() {
        let items = vec![
            make_item("item_1", "A"),
            make_separator("sep_1"),
            make_separator("sep_2"),
            make_item("item_2", "B"),
        ];
        let groups = split_items_into_groups(items);
        // Consecutive separators produce no empty groups
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0][0].title, "A");
        assert_eq!(groups[1][0].title, "B");
    }

    #[test]
    fn test_split_groups_empty_input() {
        let items: Vec<MenuJsonItem> = vec![];
        let groups = split_items_into_groups(items);
        assert!(groups.is_empty());
    }

    #[test]
    fn test_split_groups_only_separators() {
        let items = vec![
            make_separator("sep_1"),
            make_separator("sep_2"),
        ];
        let groups = split_items_into_groups(items);
        assert!(groups.is_empty());
    }

    #[test]
    fn test_split_groups_multiple_groups() {
        let items = vec![
            make_item("a", "A1"),
            make_item("b", "A2"),
            make_separator("s1"),
            make_item("c", "B1"),
            make_separator("s2"),
            make_item("d", "C1"),
            make_item("e", "C2"),
        ];
        let groups = split_items_into_groups(items);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1].len(), 1);
        assert_eq!(groups[2].len(), 2);
    }

    // ── serde-error branches for menu_to_status_bar_items / extract_menu_metadata ──

    /// Minimal ContextMenu mock that returns a fixed JSON string.
    struct MockContextMenu {
        json: String,
    }

    impl crate::menu::ContextMenu for MockContextMenu {
        fn ohos_context_menu(&self) -> String {
            self.json.clone()
        }
    }

    #[test]
    fn menu_to_status_bar_items_invalid_json_returns_none() {
        let menu: Option<Box<dyn crate::menu::ContextMenu>> =
            Some(Box::new(MockContextMenu {
                json: "{invalid json".to_string(),
            }));
        let result = menu_to_status_bar_items(&menu);
        assert!(result.is_none(), "invalid JSON should yield None");
    }

    #[test]
    fn menu_to_status_bar_items_empty_array_returns_none() {
        let menu: Option<Box<dyn crate::menu::ContextMenu>> =
            Some(Box::new(MockContextMenu {
                json: "[]".to_string(),
            }));
        let result = menu_to_status_bar_items(&menu);
        assert!(result.is_none(), "empty array should yield None");
    }

    #[test]
    fn menu_to_status_bar_items_none_menu_returns_none() {
        let menu: Option<Box<dyn crate::menu::ContextMenu>> = None;
        let result = menu_to_status_bar_items(&menu);
        assert!(result.is_none());
    }

    #[test]
    fn extract_menu_metadata_invalid_json_returns_empty() {
        let menu: Option<Box<dyn crate::menu::ContextMenu>> =
            Some(Box::new(MockContextMenu {
                json: "not json at all".to_string(),
            }));
        let (predefined, about_metadata, menu_json) = extract_menu_metadata(&menu);
        assert!(predefined.is_empty());
        assert!(about_metadata.is_empty());
        assert!(menu_json.is_none());
    }

    #[test]
    fn extract_menu_metadata_none_menu_returns_empty() {
        let menu: Option<Box<dyn crate::menu::ContextMenu>> = None;
        let (predefined, about_metadata, menu_json) = extract_menu_metadata(&menu);
        assert!(predefined.is_empty());
        assert!(about_metadata.is_empty());
        assert!(menu_json.is_none());
    }
}

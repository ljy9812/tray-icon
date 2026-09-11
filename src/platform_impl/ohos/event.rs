// OHOS StatusBar API limitations (cannot be fixed at application level):
// - No double-click detection (StatusBar callback has no "doubleClick" type)
// - No hover/enter/leave tracking (only "leftClick"/"rightClick" click_type)
// - No click position coordinates (callback provides only click_type + menuCode)
// - No middle button support (StatusBar has no "middleClick" type)
// - No button press state (callback fires on completed click only)
// Application-internal components have full OHOS gesture/mouse API support,
// but StatusBar tray icon operates through a system-level extension with
// limited callback data.

use crate::{
    dpi::PhysicalPosition, MouseButton, MouseButtonState, Rect, TrayIconEvent, TrayIconId,
};
use crossbeam_channel::{select, Receiver, Sender};
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;
use std::thread;

use openharmony_ability_plugin_statusbar::StatusBarClickEvent;

use super::MENU_METADATA;

static EVENT_THREAD_STARTED: AtomicBool = AtomicBool::new(false);
// RwLock instead of OnceCell: OHOS allows only one tray at a time (singleton),
// but multiple trays may be created/destroyed over the app lifetime.
// Each new tray must update TRAY_ID so events carry the correct ID.
static TRAY_ID: RwLock<Option<TrayIconId>> = RwLock::new(None);

// ─── Local event channels (owned by tray-icon) ──────────────────────────────
// tray-icon owns the ICON_CLICK_CHANNEL / MENU_CLICK_CHANNEL. The Senders are
// registered with plugin-statusbar (via register_icon_click_sender /
// register_menu_click_sender) so on_main_thread_event forwards decoded events
// here. start_event_forward_thread consumes from the local receivers.

static ICON_CLICK_CHANNEL: Lazy<(Sender<StatusBarClickEvent>, Receiver<StatusBarClickEvent>)> =
    Lazy::new(crossbeam_channel::unbounded);
static MENU_CLICK_CHANNEL: Lazy<(Sender<StatusBarClickEvent>, Receiver<StatusBarClickEvent>)> =
    Lazy::new(crossbeam_channel::unbounded);

/// Returns the icon-click event receiver (consumed by the event-forward thread).
pub fn icon_click_receiver() -> &'static Receiver<StatusBarClickEvent> {
    &ICON_CLICK_CHANNEL.1
}

/// Sends an icon click event into tray-icon's internal channel.
/// Used by test simulation commands to inject tray click events.
pub fn send_icon_click(click_type: String) {
    let _ = ICON_CLICK_CHANNEL
        .0
        .send(StatusBarClickEvent::IconClick { click_type });
}

/// Returns the menu-click event receiver (consumed by the event-forward thread).
pub fn menu_click_receiver() -> &'static Receiver<StatusBarClickEvent> {
    &MENU_CLICK_CHANNEL.1
}

/// Registers tray-icon's channel senders with plugin-statusbar so bridge events
/// are forwarded into the local channels. Called from set_ohos_app at startup.
pub fn register_statusbar_channels() {
    openharmony_ability_plugin_statusbar::register_icon_click_sender(ICON_CLICK_CHANNEL.0.clone());
    openharmony_ability_plugin_statusbar::register_menu_click_sender(MENU_CLICK_CHANNEL.0.clone());
}

pub fn register_tray_id(id: TrayIconId) {
    let mut guard = TRAY_ID.write().unwrap();
    *guard = Some(id);
}

pub fn get_current_tray_id() -> TrayIconId {
    TRAY_ID
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(|| TrayIconId::new("main"))
}

pub fn start_event_forward_thread() {
    if EVENT_THREAD_STARTED.swap(true, Ordering::Relaxed) {
        return;
    }

    thread::spawn(move || {
        let icon_receiver = icon_click_receiver();
        let menu_receiver = menu_click_receiver();

        loop {
            select! {
                recv(icon_receiver) -> event => {
                    if let Ok(status_bar_event) = event {
                        let tray_event = convert_icon_click(status_bar_event);
                        TrayIconEvent::send(tray_event);
                    }
                },
                recv(menu_receiver) -> event => {
                    if let Ok(status_bar_event) = event {
                        let raw_code = match &status_bar_event {
                            StatusBarClickEvent::MenuClick { menu_code } => menu_code.clone(),
                            _ => String::new(),
                        };

                        let menu_code = translate_menu_code(&raw_code);

                        let action = {
                            let metadata = MENU_METADATA.lock().unwrap();
                            if let Some(predefined_type) = metadata.predefined_map.get(&menu_code) {
                                let about_metadata = metadata.about_metadata.get(&menu_code).cloned();
                                MenuAction::Predefined { predefined_type: predefined_type.clone(), about_metadata }
                            } else {
                                MenuAction::Regular
                            }
                        };

                        match action {
                            MenuAction::Predefined { predefined_type, about_metadata } => {
                                execute_predefined_action(&predefined_type, about_metadata);
                            }
                            MenuAction::Regular => {
                                // Check items flip muda's single check-state source
                                // (it owns the Arc<AtomicBool> the user's
                                // CheckMenuItem reads); the returned state tells us
                                // what to re-push. None = not a check item.
                                if let Some(checked) = muda::toggle_check_item(&menu_code) {
                                    rebuild_menu_with_checked(&menu_code, checked);
                                } else {
                                    muda::send_menu_event(menu_code);
                                }
                            }
                        }
                    }
                },
            }
        }
    });
}

enum MenuAction {
    Predefined {
        predefined_type: String,
        about_metadata: Option<super::AboutMetadataJson>,
    },
    Regular,
}

fn execute_predefined_action(
    predefined_type: &str,
    about_metadata: Option<super::AboutMetadataJson>,
) {
    // All predefined actions — including quit — dispatch through the
    // ohos.statusbar execute-predefined bridge. quit resolves on the ArkTS
    // side: PredefinedActionExecutor case 'quit' → context.terminateSelf(),
    // the graceful OHOS ability termination. (An earlier workaround called
    // std::process::exit(0) directly from this thread; appspawn intercepts
    // direct exit() calls — faultlog "Unexpected call: exit(0)" — and
    // converts them to SIGABRT, leaving a cppcrash record on every Quit.
    // That workaround predates the StatusbarPlugin execute-predefined
    // handler and is obsolete: minimize/hide/showAll etc. prove the bridge
    // call works from this background thread.)
    match predefined_type {
        "minimize" | "hide" | "maximize" | "close" | "fullscreen" | "about"
        | "quit"
        | "copy" | "cut" | "paste" | "selectAll" | "undo" | "redo"
        | "showAll" | "bringAllToFront" => {
            // Serialized through the bridge worker like every other status-bar
            // call, keeping one FIFO order (this thread used to block_on the
            // call directly, bypassing that ordering).
            let request = openharmony_ability_plugin_statusbar::StatusBarPredefinedRequest {
                action: predefined_type.to_string(),
                about_metadata: about_metadata.map(Into::into),
            };
            super::dispatch_bridge_call(move || {
                super::with_statusbar_client("execute_predefined", |client| {
                    if let Err(e) = futures_executor::block_on(client.execute_predefined(request)) {
                        super::record_bridge_error("execute_predefined", &e);
                    }
                });
            });
        }
        // macOS-only semantics with no OHOS counterpart — muda can still emit
        // these on cross-platform menus, so they are listed explicitly as
        // no-ops instead of falling into the catch-all below.
        "hideOthers" | "services" => {}
        _ => {
            log::debug!("[TrayIcon] unsupported predefined action: {}", predefined_type);
        }
    }
}

/// Re-pushes the tray menu with `id`'s checked state patched to `checked`, so
/// the status bar reflects the flip muda's single check-state source just
/// performed. Runs on this event-forward thread (a plain Rust worker —
/// block_on here cannot deadlock the TSFN main loop).
fn rebuild_menu_with_checked(id: &str, checked: bool) {
    let json = MENU_METADATA.lock().unwrap().menu_json.clone();
    let Some(json) = json else {
        return;
    };

    fn patch_items(items: &mut [serde_json::Value], id: &str, checked: bool) -> bool {
        for item in items.iter_mut() {
            if let Some(obj) = item.as_object_mut() {
                if obj.get("id").and_then(|v| v.as_str()) == Some(id) {
                    obj.insert("checked".to_string(), serde_json::Value::Bool(checked));
                    return true;
                }
                if let Some(sub) = obj.get_mut("submenuItems") {
                    if let Some(arr) = sub.as_array_mut() {
                        if patch_items(arr, id, checked) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    let mut items: Vec<serde_json::Value> = match serde_json::from_str(&json) {
        Ok(v) => v,
        Err(_) => return,
    };
    patch_items(&mut items, id, checked);

    let patched_json = match serde_json::to_string(&items) {
        Ok(j) => j,
        Err(_) => return,
    };

    let menu_items: Vec<super::MenuJsonItem> = match serde_json::from_str(&patched_json) {
        Ok(v) => v,
        Err(_) => return,
    };

    let mut groups = super::split_items_into_groups(menu_items);

    let flat_ids = super::remap_menu_codes_to_indices(&mut groups);
    MENU_METADATA.lock().unwrap().flat_ids = flat_ids;

    if !groups.is_empty() {
        let request = openharmony_ability_plugin_statusbar::StatusBarUpdateMenuRequest::from(&groups);
        super::with_statusbar_client("update_menu", |client| {
            if let Err(e) = futures_executor::block_on(client.update_menu(request)) {
                super::record_bridge_error("update_menu", &e);
            }
        });
    }
}

fn convert_icon_click(
    event: StatusBarClickEvent,
) -> TrayIconEvent {
    let button = match event {
        StatusBarClickEvent::IconClick { click_type } => {
            match click_type.as_str() {
                "rightClick" => MouseButton::Right,
                unknown => {
                    log::debug!("[TrayIcon] unknown click_type '{}', defaulting to Left", unknown);
                    MouseButton::Left
                }
            }
        }
        other => {
            log::debug!("[TrayIcon] unexpected event variant {:?}, defaulting to Left click", other);
            MouseButton::Left
        }
    };

    TrayIconEvent::Click {
        id: get_current_tray_id(),
        position: PhysicalPosition::new(0.0, 0.0),
        rect: Rect::default(),
        button,
        button_state: MouseButtonState::Up,
    }
}

/// Translate a system-returned numeric menuCode back to our original string ID.
fn translate_menu_code(raw_code: &str) -> String {
    let metadata = MENU_METADATA.lock().unwrap();

    if metadata.flat_ids.is_empty() {
        return raw_code.to_string();
    }

    match raw_code.parse::<usize>() {
        Ok(idx) if idx < metadata.flat_ids.len() => {
            metadata.flat_ids[idx].clone()
        }
        _ => raw_code.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_icon_click_left() {
        let event = StatusBarClickEvent::IconClick {
            click_type: "leftClick".to_string(),
        };
        let tray_event = convert_icon_click(event);

        match tray_event {
            TrayIconEvent::Click { button, .. } => {
                assert_eq!(button, MouseButton::Left);
            }
            _ => panic!("unexpected event type"),
        }
    }

    #[test]
    fn test_icon_click_right() {
        let event = StatusBarClickEvent::IconClick {
            click_type: "rightClick".to_string(),
        };
        let tray_event = convert_icon_click(event);

        match tray_event {
            TrayIconEvent::Click { button, .. } => {
                assert_eq!(button, MouseButton::Right);
            }
            _ => panic!("unexpected event type"),
        }
    }

    #[test]
    fn test_icon_click_unknown_type_defaults_left() {
        let event = StatusBarClickEvent::IconClick {
            click_type: "middleClick".to_string(),
        };
        let tray_event = convert_icon_click(event);
        match tray_event {
            TrayIconEvent::Click { button, .. } => {
                assert_eq!(button, MouseButton::Left);
            }
            _ => panic!("unexpected event type"),
        }
    }

    #[test]
    fn test_menu_click_event_defaults_left() {
        let event = StatusBarClickEvent::MenuClick {
            menu_code: "item_0".to_string(),
        };
        let tray_event = convert_icon_click(event);
        match tray_event {
            TrayIconEvent::Click { button, .. } => {
                assert_eq!(button, MouseButton::Left);
            }
            _ => panic!("unexpected event type"),
        }
    }
}

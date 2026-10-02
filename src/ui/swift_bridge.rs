// Copyright The Sugarglider Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! FFI bridge to SugargliderUI Swift library.
//!
//! This module provides Rust bindings to call Swift UI functions
//! for the preferences window and drop zone overlay. It also provides
//! C-callable functions for Swift to read and update configuration.

use std::ffi::{CStr, CString, c_char};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use crate::actor::contexts_snapshot::{self, CONTEXTS_OFF};
use crate::actor::reactor::Command;
use crate::actor::wm_controller::{WmCommand, WmEvent};
use crate::config::{self, Config};
use crate::ui::context_switcher::{SwitcherCommand, rank_snapshot};
use crate::ui::preferences_json::PreferencesJson;

/// Track whether the Swift UI library is available
static SWIFT_UI_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Global config state accessible from FFI functions.
/// Updated by WmController on ConfigUpdated events.
static CURRENT_CONFIG: OnceLock<RwLock<Arc<Config>>> = OnceLock::new();

/// Set the current config. Called by WmController when config changes.
pub fn set_current_config(config: Arc<Config>) {
    match CURRENT_CONFIG.get() {
        Some(lock) => {
            *lock.write().unwrap() = config;
        }
        None => {
            let _ = CURRENT_CONFIG.set(RwLock::new(config));
        }
    }
}

/// Get the current config.
pub fn get_current_config() -> Option<Arc<Config>> {
    CURRENT_CONFIG.get().map(|lock| lock.read().unwrap().clone())
}

// FFI declarations for Swift functions
#[cfg(feature = "swift-ui")]
unsafe extern "C" {
    /// Shows the preferences window (defined in SugargliderUI.swift)
    fn sugarglider_show_preferences();

    /// Hides the preferences window (defined in SugargliderUI.swift)
    fn sugarglider_hide_preferences();

    /// Shows drop zone overlays (defined in DropZoneOverlay.swift)
    fn sugarglider_show_drop_zones(zones_ptr: *const f32, count: i32);

    /// Hides drop zone overlays (defined in DropZoneOverlay.swift)
    fn sugarglider_hide_drop_zones();

    fn sugarglider_show_context_switcher(json: *const c_char);
    fn sugarglider_hide_context_switcher();

    /// Shows a transient size share badge (defined in SizeShareBadge.swift)
    fn sugarglider_show_size_share_badge(
        text: *const c_char,
        kind: i32,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        screen_index: i32,
    );
}

/// Initialize the Swift UI bridge.
/// Call this once at startup to verify the Swift library is loaded.
pub fn init() -> bool {
    #[cfg(feature = "swift-ui")]
    {
        // Force the linker to keep the FFI functions by referencing them.
        // This prevents dead code elimination since these functions are only
        // called from Swift, not from Rust.
        std::hint::black_box(sugarglider_get_config as *const () as usize);
        std::hint::black_box(sugarglider_update_config as *const () as usize);
        std::hint::black_box(sugarglider_save_config_to_file as *const () as usize);
        std::hint::black_box(sugarglider_free_string as *const () as usize);
        std::hint::black_box(sugarglider_rank_contexts as *const () as usize);
        std::hint::black_box(sugarglider_run_context_command as *const () as usize);

        // The library is linked at compile time, so if we got here it's available
        SWIFT_UI_AVAILABLE.store(true, Ordering::SeqCst);
        tracing::info!("SugargliderUI Swift library initialized");
        true
    }

    #[cfg(not(feature = "swift-ui"))]
    {
        tracing::warn!(
            "SugargliderUI Swift library not available (compiled without swift-ui feature)"
        );
        false
    }
}

/// Check if Swift UI is available
pub fn is_available() -> bool {
    SWIFT_UI_AVAILABLE.load(Ordering::SeqCst)
}

/// Sends a fresh show payload to Swift, which copies it before returning.
pub fn show_context_switcher(json: String) {
    #[cfg(feature = "swift-ui")]
    if is_available()
        && let Ok(json) = CString::new(json)
    {
        unsafe { sugarglider_show_context_switcher(json.as_ptr()) };
    }
    #[cfg(not(feature = "swift-ui"))]
    let _ = json;
}

/// Hides the panel, including when a context action came from another source.
pub fn hide_context_switcher() {
    #[cfg(feature = "swift-ui")]
    if is_available() {
        unsafe { sugarglider_hide_context_switcher() };
    }
}

/// Returns ranked contexts from the last published snapshot. No actor reply
/// is needed while the panel waits on the main thread.
///
/// # Safety
///
/// `query` must be null or a valid NUL-terminated C string that stays alive
/// for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sugarglider_rank_contexts(query: *const c_char) -> *mut c_char {
    if query.is_null() {
        return std::ptr::null_mut();
    }
    let Ok(query) = (unsafe { CStr::from_ptr(query) }).to_str() else {
        return std::ptr::null_mut();
    };
    let Some(snapshot) = contexts_snapshot::published()
        .filter(|snapshot| snapshot.enabled && snapshot.shown().is_some())
    else {
        return std::ptr::null_mut();
    };
    serde_json::to_string(&rank_snapshot(query, &snapshot))
        .ok()
        .and_then(|json| CString::new(json).ok())
        .map_or(std::ptr::null_mut(), CString::into_raw)
}

/// Checks the command against the published snapshot and queues it without
/// waiting for the reactor or WmController to execute it.
///
/// # Safety
///
/// `json` must be null or a valid NUL-terminated C string that stays alive for
/// the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sugarglider_run_context_command(json: *const c_char) -> *mut c_char {
    if json.is_null() {
        return error_string("Null command pointer");
    }
    let Ok(json) = (unsafe { CStr::from_ptr(json) }).to_str() else {
        return error_string("Invalid UTF-8 in context command");
    };
    let command: SwitcherCommand = match serde_json::from_str(json) {
        Ok(command) => command,
        Err(err) => return error_string(&format!("Invalid context command: {err}")),
    };
    let Some(snapshot) = contexts_snapshot::published().filter(|snapshot| snapshot.enabled) else {
        return error_string(CONTEXTS_OFF);
    };
    if snapshot.shown().is_none() {
        return error_string("No Space is managed right now");
    }
    let command = match command.checked(&snapshot) {
        Ok(command) => command,
        Err(err) => return error_string(&err),
    };
    let Some(sender) = CONFIG_UPDATE_SENDER.get() else {
        return error_string("The context command channel is unavailable");
    };
    let Ok(sender) = sender.try_lock() else {
        return error_string("The context command channel is busy");
    };
    let Some(sender) = sender.as_ref() else {
        return error_string("The context command channel is unavailable");
    };
    match sender.send((
        tracing::Span::current(),
        WmEvent::Command(WmCommand::ReactorCommand(Command::Context(command))),
    )) {
        Ok(()) => std::ptr::null_mut(),
        Err(_) => error_string("The context command channel is closed"),
    }
}

/// Show the preferences window.
///
/// This opens a native SwiftUI preferences window with tabs for
/// General, Layouts, Hotkeys, App Rules, and About.
pub fn show_preferences() {
    #[cfg(feature = "swift-ui")]
    {
        if is_available() {
            tracing::debug!("Opening preferences window");
            unsafe { sugarglider_show_preferences() };
        } else {
            tracing::warn!("Cannot show preferences: Swift UI not available");
        }
    }

    #[cfg(not(feature = "swift-ui"))]
    {
        tracing::warn!("Preferences window not available (compiled without swift-ui feature)");
        // Fallback: open config file in default editor
        if let Err(e) = open_config_file() {
            tracing::error!("Failed to open config file: {e}");
        }
    }
}

/// Hide the preferences window.
pub fn hide_preferences() {
    #[cfg(feature = "swift-ui")]
    {
        if is_available() {
            tracing::debug!("Closing preferences window");
            unsafe { sugarglider_hide_preferences() };
        }
    }
}

/// Action type for drop zones
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DropActionType {
    /// Swap window assignments (safest)
    Swap = 0,
    /// Insert as sibling
    Insert = 1,
    /// Split into new container
    Split = 2,
}

/// Represents a drop zone for drag-and-drop window rearrangement
#[derive(Debug, Clone, Copy)]
pub struct DropZone {
    /// X coordinate of the zone (screen-local)
    pub x: f32,
    /// Y coordinate of the zone (screen-local)
    pub y: f32,
    /// Width of the zone
    pub width: f32,
    /// Height of the zone
    pub height: f32,
    /// Type of drop position (0=left, 1=right, 2=top, 3=bottom, 4=tab, 5=new_column)
    pub position_type: i32,
    /// Target window ID for the drop
    pub target_window_id: u64,
    /// Action type (0=swap, 1=insert, 2=split)
    pub action_type: DropActionType,
    /// Whether this zone is currently active (hovered)
    pub is_active: bool,
    /// Dwell progress for split zones (0.0-1.0)
    pub dwell_progress: f32,
    /// Screen index this zone belongs to
    pub screen_index: i32,
}

impl DropZone {
    /// Create a new drop zone
    pub fn new(
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        position_type: i32,
        target_window_id: u64,
    ) -> Self {
        Self {
            x,
            y,
            width,
            height,
            position_type,
            target_window_id,
            action_type: DropActionType::Swap,
            is_active: false,
            dwell_progress: 0.0,
            screen_index: 0,
        }
    }

    /// Create a drop zone with extended info
    pub fn with_action(
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        position_type: i32,
        target_window_id: u64,
        action_type: DropActionType,
        is_active: bool,
        dwell_progress: f32,
        screen_index: i32,
    ) -> Self {
        Self {
            x,
            y,
            width,
            height,
            position_type,
            target_window_id,
            action_type,
            is_active,
            dwell_progress,
            screen_index,
        }
    }

    /// Convert to flat array for FFI (10 floats per zone)
    fn to_floats(&self) -> [f32; 10] {
        [
            self.x,
            self.y,
            self.width,
            self.height,
            self.position_type as f32,
            self.target_window_id as f32,
            self.action_type as u8 as f32,
            if self.is_active { 1.0 } else { 0.0 },
            self.dwell_progress,
            self.screen_index as f32,
        ]
    }
}

/// Show drop zone overlay with the specified zones.
///
/// Call this when a window drag starts to show visual indicators
/// of where the window can be dropped.
pub fn show_drop_zones(zones: &[DropZone]) {
    #[cfg(feature = "swift-ui")]
    {
        if !is_available() || zones.is_empty() {
            return;
        }

        // Flatten zones to f32 array for FFI
        let flat: Vec<f32> = zones.iter().flat_map(|z| z.to_floats()).collect();

        tracing::debug!("Showing {} drop zones", zones.len());
        unsafe {
            sugarglider_show_drop_zones(flat.as_ptr(), zones.len() as i32);
        }
    }

    #[cfg(not(feature = "swift-ui"))]
    {
        let _ = zones;
        tracing::trace!("Drop zones not available (compiled without swift-ui feature)");
    }
}

/// Hide the drop zone overlay.
///
/// Call this when a window drag ends or is cancelled.
pub fn hide_drop_zones() {
    #[cfg(feature = "swift-ui")]
    {
        if is_available() {
            tracing::debug!("Hiding drop zones");
            unsafe { sugarglider_hide_drop_zones() };
        }
    }
}

/// How a size share badge should be styled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum SizeShareBadgeKind {
    Applied = 0,
    Released = 1,
    Rejected = 2,
}

/// Shows a transient badge with the window's new size share.
///
/// `frame` is the window's frame in screen-local coordinates, used to place
/// the badge near the window.
pub fn show_size_share_badge(
    text: &str,
    kind: SizeShareBadgeKind,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    screen_index: i32,
) {
    #[cfg(feature = "swift-ui")]
    {
        if !is_available() {
            return;
        }
        let Ok(text) = CString::new(text) else {
            return;
        };
        tracing::debug!("Showing size share badge {text:?} at {x},{y} {width}x{height}");
        unsafe {
            sugarglider_show_size_share_badge(
                text.as_ptr(),
                kind as i32,
                x,
                y,
                width,
                height,
                screen_index,
            );
        }
    }

    #[cfg(not(feature = "swift-ui"))]
    {
        let _ = (text, kind, x, y, width, height, screen_index);
        tracing::trace!("Size share badge not available (compiled without swift-ui feature)");
    }
}

// ============================================================================
// C-callable FFI functions for Swift to access configuration
// ============================================================================

/// Get the current configuration as a JSON string.
///
/// Returns a pointer to a null-terminated JSON string, or null if no config
/// is available. The caller must free the returned string using
/// `sugarglider_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn sugarglider_get_config() -> *mut c_char {
    let Some(config) = get_current_config() else {
        tracing::warn!("sugarglider_get_config: No config available");
        return std::ptr::null_mut();
    };

    let prefs = PreferencesJson::from_config(&config);

    match serde_json::to_string(&prefs) {
        Ok(json) => match CString::new(json) {
            Ok(cstr) => cstr.into_raw(),
            Err(e) => {
                tracing::error!("Failed to create CString: {e}");
                std::ptr::null_mut()
            }
        },
        Err(e) => {
            tracing::error!("Failed to serialize config to JSON: {e}");
            std::ptr::null_mut()
        }
    }
}

/// Update the running window manager with new configuration.
///
/// Takes a JSON string representing the preferences. Returns null on success,
/// or a pointer to an error message string on failure. The caller must free
/// any returned error string using `sugarglider_free_string`.
///
/// # Safety
///
/// `json_ptr` must be null or a valid NUL-terminated C string that stays alive
/// for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sugarglider_update_config(json_ptr: *const c_char) -> *mut c_char {
    if json_ptr.is_null() {
        return error_string("Null JSON pointer");
    }

    let json_str = match unsafe { CStr::from_ptr(json_ptr) }.to_str() {
        Ok(s) => s,
        Err(e) => return error_string(&format!("Invalid UTF-8: {e}")),
    };

    let prefs: PreferencesJson = match serde_json::from_str(json_str) {
        Ok(p) => p,
        Err(e) => return error_string(&format!("Invalid JSON: {e}")),
    };

    let Some(current_config) = get_current_config() else {
        return error_string("No current config available");
    };

    let new_config = prefs.apply_to_config(&current_config);
    let new_config = Arc::new(new_config);

    // Update the global config state
    set_current_config(new_config.clone());

    // Send config update through the WmController channel
    // This is handled by the config_update_sender if set
    if let Some(sender) = CONFIG_UPDATE_SENDER.get() {
        if let Some(sender) = sender.lock().unwrap().as_ref() {
            use crate::actor::wm_controller::WmEvent;
            let _ = sender.send((tracing::Span::current(), WmEvent::ConfigUpdated(new_config)));
        }
    }

    tracing::info!("Config updated via preferences UI");
    std::ptr::null_mut() // Success
}

/// Save configuration to the TOML config file.
///
/// Takes a JSON string representing the preferences. Returns null on success,
/// or a pointer to an error message string on failure.
///
/// # Safety
///
/// `json_ptr` must be null or a valid NUL-terminated C string that stays alive
/// for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sugarglider_save_config_to_file(json_ptr: *const c_char) -> *mut c_char {
    if json_ptr.is_null() {
        return error_string("Null JSON pointer");
    }

    let json_str = match unsafe { CStr::from_ptr(json_ptr) }.to_str() {
        Ok(s) => s,
        Err(e) => return error_string(&format!("Invalid UTF-8: {e}")),
    };

    let prefs: PreferencesJson = match serde_json::from_str(json_str) {
        Ok(p) => p,
        Err(e) => return error_string(&format!("Invalid JSON: {e}")),
    };

    match config::write_preferences_to_file(&prefs) {
        Ok(path) => {
            tracing::info!("Config saved to {}", path.display());
            std::ptr::null_mut() // Success
        }
        Err(e) => error_string(&format!("Failed to save config: {e}")),
    }
}

/// Free a string returned by other FFI functions.
///
/// # Safety
///
/// `ptr` must be null or a pointer returned by one of these functions that has
/// not been freed yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sugarglider_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            drop(CString::from_raw(ptr));
        }
    }
}

/// Helper to create an error string for FFI return.
fn error_string(msg: &str) -> *mut c_char {
    tracing::error!("{msg}");
    CString::new(msg).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// Channel sender for config updates, set during initialization.
static CONFIG_UPDATE_SENDER: OnceLock<
    std::sync::Mutex<Option<crate::actor::wm_controller::Sender>>,
> = OnceLock::new();

/// Set the sender for config updates. Called during WmController initialization.
pub fn set_config_update_sender(sender: crate::actor::wm_controller::Sender) {
    match CONFIG_UPDATE_SENDER.get() {
        Some(lock) => {
            *lock.lock().unwrap() = Some(sender);
        }
        None => {
            let _ = CONFIG_UPDATE_SENDER.set(std::sync::Mutex::new(Some(sender)));
        }
    }
}

/// Fallback: open config file in default text editor
#[cfg(not(feature = "swift-ui"))]
fn open_config_file() -> std::io::Result<()> {
    use std::process::Command;

    let config_path = dirs::config_dir()
        .map(|p| p.join("sugarglider").join("sugarglider.toml"))
        .or_else(|| dirs::home_dir().map(|p| p.join(".sugarglider.toml")));

    if let Some(path) = config_path {
        if path.exists() {
            Command::new("/usr/bin/open").arg("-t").arg(&path).spawn()?;
        } else {
            // Open the default config as reference
            let default_config = concat!(env!("CARGO_MANIFEST_DIR"), "/sugarglider.default.toml");
            Command::new("/usr/bin/open").arg("-t").arg(default_config).spawn()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drop_zone_to_floats() {
        let zone = DropZone::new(100.0, 200.0, 50.0, 50.0, 1, 12345);
        let floats = zone.to_floats();
        assert_eq!(floats[0], 100.0);
        assert_eq!(floats[1], 200.0);
        assert_eq!(floats[2], 50.0);
        assert_eq!(floats[3], 50.0);
        assert_eq!(floats[4], 1.0);
        assert_eq!(floats[5], 12345.0);
    }
}

// Copyright The Sugarglider Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! JSON types for preferences UI communication.
//!
//! This module defines types that are serialized to JSON for exchange between
//! the Swift preferences UI and the Rust backend.

use livesplit_hotkey::{Hotkey, KeyCode, Modifiers};
use serde::{Deserialize, Serialize};

use crate::actor::layout::{LayoutCommand, SizeShare};
use crate::actor::reactor::{
    Command as ReactorCommand, ContextCommand, ContextRef, ReactorCommand as ReactorCmd,
};
use crate::actor::wm_controller::{WmCmd, WmCommand};
use crate::config::{Config, ConfigRegex, WindowRule, WindowRuleConditions};
use crate::log::MetricsCommand;
use crate::model::contexts::Scope;
use crate::model::{Direction, LayoutKind, Orientation};

/// Subset of Config fields editable via the preferences UI.
///
/// Uses JSON-friendly field names (snake_case maps to Swift camelCase).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PreferencesJson {
    // General settings
    pub status_icon_enable: bool,
    pub animate: bool,
    pub focus_follows_mouse: bool,
    pub mouse_follows_focus: bool,
    pub outer_gap: f64,
    pub inner_gap: f64,

    // Dragging behavior
    pub drag_drop_enable: bool,
    pub drag_drop_live_preview: bool,

    // Layout settings
    pub default_layout_kind: String,

    // Experimental features
    pub contexts_enable: bool,
    pub contexts_scope: Scope,

    // Window rules
    pub window_rules: Vec<WindowRuleJson>,

    // Hotkey bindings (read-only for now)
    pub hotkeys: Vec<HotkeyBindingJson>,
}

/// JSON representation of a hotkey binding.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyBindingJson {
    /// The formatted hotkey string (e.g., "⌥H")
    pub key: String,
    /// The bound command as JSON, arguments included. The Swift UI sends it
    /// back unchanged, so each binding keeps its own command.
    pub command: String,
    /// Human-readable description of what the command does
    pub description: String,
    /// Category for grouping in the UI
    pub category: String,
    /// The default hotkey for this command (if any)
    pub default_key: Option<String>,
    /// Sort order within the category (lower = earlier). The Swift UI doesn't
    /// send it back.
    #[serde(default)]
    pub sort_order: u32,
}

/// JSON representation of a window rule.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WindowRuleJson {
    pub app_name: Option<String>,
    pub bundle_id: Option<String>,
    pub behavior: String, // "tile" or "float"
    /// The conditions the App Rules pane doesn't show. The Swift UI carries
    /// them through unchanged, so a save keeps them.
    pub title_regex: Option<String>,
    pub title_substring: Option<String>,
    pub ax_role: Option<String>,
    pub ax_subrole: Option<String>,
}

impl PreferencesJson {
    /// Create PreferencesJson from a Config.
    pub fn from_config(config: &Config) -> Self {
        let settings = &config.settings;

        Self {
            status_icon_enable: settings.status_icon.enable,
            animate: settings.animate,
            focus_follows_mouse: settings.focus_follows_mouse,
            mouse_follows_focus: settings.mouse_follows_focus,
            outer_gap: settings.outer_gap,
            inner_gap: settings.inner_gap,
            drag_drop_enable: settings.drag_drop.enable,
            drag_drop_live_preview: settings.drag_drop.live_preview,
            default_layout_kind: match settings.default_layout_kind {
                LayoutKind::Tree => "tree".to_string(),
                LayoutKind::Scroll => "scroll".to_string(),
            },
            contexts_enable: settings.experimental.contexts.enable,
            contexts_scope: settings.experimental.contexts.scope,
            window_rules: config.window_rules.iter().map(WindowRuleJson::from_rule).collect(),
            hotkeys: {
                // The default hotkey of each command in the default config
                let default_keys: Vec<(serde_json::Value, String)> = Config::default()
                    .keys
                    .iter()
                    .map(|(hotkey, cmd)| (command_json(cmd), format_hotkey(hotkey)))
                    .collect();

                let mut hotkeys: Vec<_> = config
                    .keys
                    .iter()
                    .map(|(hotkey, cmd)| {
                        let command = command_json(cmd);
                        let default_key = default_keys
                            .iter()
                            .find(|(default_command, _)| *default_command == command)
                            .map(|(_, key)| key.clone());
                        HotkeyBindingJson::from_binding_with_default(hotkey, cmd, default_key)
                    })
                    .collect();

                // Sort by category, then by sort_order within each category
                hotkeys.sort_by(|a, b| {
                    a.category.cmp(&b.category).then_with(|| a.sort_order.cmp(&b.sort_order))
                });

                hotkeys
            },
        }
    }

    /// Apply this preferences JSON to a Config, returning a new Config.
    ///
    /// Fields not present in PreferencesJson are preserved from the original.
    pub fn apply_to_config(&self, config: &Config) -> Config {
        let mut settings = config.settings.clone();

        settings.status_icon.enable = self.status_icon_enable;
        settings.animate = self.animate;
        settings.focus_follows_mouse = self.focus_follows_mouse;
        settings.mouse_follows_focus = self.mouse_follows_focus;
        settings.outer_gap = self.outer_gap;
        settings.inner_gap = self.inner_gap;
        settings.drag_drop.enable = self.drag_drop_enable;
        settings.drag_drop.live_preview = self.drag_drop_live_preview;
        settings.default_layout_kind = match self.default_layout_kind.as_str() {
            "scroll" => LayoutKind::Scroll,
            _ => LayoutKind::Tree,
        };
        settings.experimental.contexts.enable = self.contexts_enable;
        settings.experimental.contexts.scope = self.contexts_scope;

        let window_rules: Vec<WindowRule> =
            self.window_rules.iter().filter_map(WindowRuleJson::to_rule).collect();

        let keys = self.bindings();

        Config { settings, window_rules, keys }
    }

    /// The key bindings, each with the command it carries. A binding whose
    /// key or command doesn't parse is dropped. When two rows name the same
    /// hotkey, the later row wins, as in a TOML table.
    pub fn bindings(&self) -> Vec<(Hotkey, WmCommand)> {
        let mut bindings: Vec<(Hotkey, WmCommand)> = Vec::new();
        for binding in self.hotkeys.iter().filter_map(HotkeyBindingJson::binding) {
            bindings.retain(|(hotkey, _)| *hotkey != binding.0);
            bindings.push(binding);
        }
        bindings
    }
}

/// A command in the form that [`HotkeyBindingJson::command`] carries and that
/// the config file stores. Two commands are the same when these are equal.
pub fn command_json(cmd: &WmCommand) -> serde_json::Value {
    serde_json::to_value(cmd).expect("commands serialize to JSON")
}

/// The modifiers of a hotkey string, in the order the Preferences window
/// shows them, each with its macOS symbol. [`format_hotkey`] writes them and
/// [`parse_hotkey_string`] reads them.
const MODIFIER_SYMBOLS: &[(Modifiers, char)] = &[
    (Modifiers::CONTROL, '⌃'),
    (Modifiers::ALT, '⌥'),
    (Modifiers::SHIFT, '⇧'),
    (Modifiers::META, '⌘'),
];

/// The keys whose text in the Preferences window differs from the name
/// [`KeyCode::name`] gives them, with the text the window shows. Every other
/// key keeps its name (e.g. "F1", "Numpad1").
const KEY_TEXT: &[(KeyCode, &str)] = &[
    (KeyCode::Backquote, "`"),
    (KeyCode::Backslash, "\\"),
    (KeyCode::BracketLeft, "["),
    (KeyCode::BracketRight, "]"),
    (KeyCode::Comma, ","),
    (KeyCode::Digit0, "0"),
    (KeyCode::Digit1, "1"),
    (KeyCode::Digit2, "2"),
    (KeyCode::Digit3, "3"),
    (KeyCode::Digit4, "4"),
    (KeyCode::Digit5, "5"),
    (KeyCode::Digit6, "6"),
    (KeyCode::Digit7, "7"),
    (KeyCode::Digit8, "8"),
    (KeyCode::Digit9, "9"),
    (KeyCode::Equal, "="),
    (KeyCode::KeyA, "A"),
    (KeyCode::KeyB, "B"),
    (KeyCode::KeyC, "C"),
    (KeyCode::KeyD, "D"),
    (KeyCode::KeyE, "E"),
    (KeyCode::KeyF, "F"),
    (KeyCode::KeyG, "G"),
    (KeyCode::KeyH, "H"),
    (KeyCode::KeyI, "I"),
    (KeyCode::KeyJ, "J"),
    (KeyCode::KeyK, "K"),
    (KeyCode::KeyL, "L"),
    (KeyCode::KeyM, "M"),
    (KeyCode::KeyN, "N"),
    (KeyCode::KeyO, "O"),
    (KeyCode::KeyP, "P"),
    (KeyCode::KeyQ, "Q"),
    (KeyCode::KeyR, "R"),
    (KeyCode::KeyS, "S"),
    (KeyCode::KeyT, "T"),
    (KeyCode::KeyU, "U"),
    (KeyCode::KeyV, "V"),
    (KeyCode::KeyW, "W"),
    (KeyCode::KeyX, "X"),
    (KeyCode::KeyY, "Y"),
    (KeyCode::KeyZ, "Z"),
    (KeyCode::Minus, "-"),
    (KeyCode::Period, "."),
    (KeyCode::Quote, "'"),
    (KeyCode::Semicolon, ";"),
    (KeyCode::Slash, "/"),
    (KeyCode::Backspace, "⌫"),
    (KeyCode::Enter, "↩"),
    (KeyCode::Escape, "Esc"),
    (KeyCode::Space, "Space"),
    (KeyCode::Tab, "⇥"),
    (KeyCode::ArrowDown, "↓"),
    (KeyCode::ArrowLeft, "←"),
    (KeyCode::ArrowRight, "→"),
    (KeyCode::ArrowUp, "↑"),
];

/// The text the Preferences window shows for a key.
fn key_text(key: KeyCode) -> &'static str {
    for &(entry, text) in KEY_TEXT {
        if entry == key {
            return text;
        }
    }
    key.name()
}

/// The key whose window text is `text`, or `None` when no key reads it. A key
/// the window shows by name (e.g. "F1") reads through the loader's names.
fn key_from_text(text: &str) -> Option<KeyCode> {
    for &(key, name) in KEY_TEXT {
        if name == text {
            return Some(key);
        }
    }
    text.parse::<KeyCode>().ok()
}

/// The hotkey as the Preferences window shows it, e.g. "⌥⇧H". The inverse of
/// [`parse_hotkey_string`].
pub(crate) fn format_hotkey(hotkey: &Hotkey) -> String {
    let mut result = String::new();
    for &(modifier, symbol) in MODIFIER_SYMBOLS {
        if hotkey.modifiers.contains(modifier) {
            result.push(symbol);
        }
    }
    result.push_str(key_text(hotkey.key_code));
    result
}

/// The hotkey that the window text `s` shows, e.g. "⌥⇧H". The inverse of
/// [`format_hotkey`]; a key without a modifier is a hotkey too.
fn parse_hotkey_string(s: &str) -> Option<Hotkey> {
    let mut modifiers = Modifiers::empty();
    let mut rest = s;
    'modifiers: loop {
        for &(modifier, symbol) in MODIFIER_SYMBOLS {
            if let Some(stripped) = rest.strip_prefix(symbol) {
                modifiers.insert(modifier);
                rest = stripped;
                continue 'modifiers;
            }
        }
        break;
    }
    if rest.is_empty() {
        return None;
    }
    let key_code = key_from_text(rest)?;
    Some(Hotkey { key_code, modifiers })
}

impl WindowRuleJson {
    /// Create from a WindowRule.
    pub fn from_rule(rule: &WindowRule) -> Self {
        Self {
            app_name: rule.conditions.app_name.clone(),
            bundle_id: rule.conditions.app_id.clone(),
            behavior: if rule.float { "float" } else { "tile" }.to_string(),
            title_regex: rule
                .conditions
                .title_regex
                .as_ref()
                .map(|regex| regex.as_str().to_string()),
            title_substring: rule.conditions.title_substring.clone(),
            ax_role: rule.conditions.ax_role.clone(),
            ax_subrole: rule.conditions.ax_subrole.clone(),
        }
    }

    /// Convert to a WindowRule. `None` when a title regex doesn't compile:
    /// the rule is dropped rather than widened to every window.
    pub fn to_rule(&self) -> Option<WindowRule> {
        let title_regex = match &self.title_regex {
            Some(pattern) => match pattern.parse::<ConfigRegex>() {
                Ok(regex) => Some(regex),
                Err(e) => {
                    tracing::warn!(
                        "Dropping window rule with an invalid title regex {pattern}: {e}"
                    );
                    return None;
                }
            },
            None => None,
        };
        Some(WindowRule {
            conditions: WindowRuleConditions {
                app_id: self.bundle_id.clone(),
                app_name: self.app_name.clone(),
                title_regex,
                title_substring: self.title_substring.clone(),
                ax_role: self.ax_role.clone(),
                ax_subrole: self.ax_subrole.clone(),
            },
            float: self.behavior == "float",
        })
    }
}

impl HotkeyBindingJson {
    /// Create from a hotkey binding with an optional default key.
    pub fn from_binding_with_default(
        hotkey: &Hotkey,
        cmd: &WmCommand,
        default_key: Option<String>,
    ) -> Self {
        let (description, category, sort_order) = describe_command(cmd);
        Self {
            key: format_hotkey(hotkey),
            command: command_json(cmd).to_string(),
            description,
            category,
            default_key,
            sort_order,
        }
    }

    /// The hotkey and the command of this binding, or `None` if either
    /// doesn't parse.
    pub fn binding(&self) -> Option<(Hotkey, WmCommand)> {
        let Some(hotkey) = parse_hotkey_string(&self.key) else {
            tracing::warn!(
                "Invalid hotkey format '{}' for command {}, dropping binding",
                self.key,
                self.command
            );
            return None;
        };
        match serde_json::from_str(&self.command) {
            Ok(cmd) => Some((hotkey, cmd)),
            Err(e) => {
                tracing::warn!(
                    "Invalid command {} for hotkey '{}', dropping binding: {e}",
                    self.command,
                    self.key
                );
                None
            }
        }
    }
}

/// Get the description, category, and sort order for a WmCommand.
fn describe_command(cmd: &WmCommand) -> (String, String, u32) {
    match cmd {
        WmCommand::Wm(wm_cmd) => match wm_cmd {
            WmCmd::ToggleGlobalEnabled => {
                ("Toggle tiling globally".to_string(), "System".to_string(), 0)
            }
            WmCmd::SetGlobalEnabled(enabled) => (
                format!("Set tiling {}", if *enabled { "on" } else { "off" }),
                "System".to_string(),
                1,
            ),
            WmCmd::ToggleSpaceActivated => (
                "Toggle tiling on current space".to_string(),
                "System".to_string(),
                2,
            ),
            WmCmd::Exec(_) => ("Execute command".to_string(), "Utilities".to_string(), 0),
        },
        WmCommand::ReactorCommand(reactor_cmd) => match reactor_cmd {
            ReactorCommand::Layout(layout_cmd) => describe_layout_command(layout_cmd),
            ReactorCommand::Metrics(metrics_cmd) => match metrics_cmd {
                MetricsCommand::ShowTiming => {
                    ("Show performance timing".to_string(), "Developer".to_string(), 0)
                }
            },
            ReactorCommand::Reactor(reactor_cmd) => match reactor_cmd {
                ReactorCmd::Debug => {
                    ("Print layout debug info".to_string(), "Developer".to_string(), 0)
                }
                ReactorCmd::Serialize => {
                    ("Serialize layout state".to_string(), "Developer".to_string(), 1)
                }
                ReactorCmd::SaveAndExit => {
                    ("Save state and exit".to_string(), "System".to_string(), 10)
                }
            },
            ReactorCommand::Context(context_cmd) => describe_context_command(context_cmd),
        },
    }
}

/// Get the description, category, and sort order for a ContextCommand.
fn describe_context_command(cmd: &ContextCommand) -> (String, String, u32) {
    let category = "Contexts".to_string();
    match cmd {
        ContextCommand::ShowEverything => ("Show every window".to_string(), category, 0),
        ContextCommand::OpenContextSwitcher => {
            ("Open the context switcher".to_string(), category, 11)
        }
        ContextCommand::SwitchContext(ContextRef::Number(number)) => (
            format!("Switch to context {number}"),
            category,
            u32::from(*number),
        ),
        ContextCommand::PreviousContext => {
            ("Switch to the previous context".to_string(), category, 10)
        }
        ContextCommand::SwitchContext(ContextRef::Name(name)) => {
            (format!("Switch to context \"{name}\""), category, 20)
        }
        ContextCommand::SwitchContext(ContextRef::Id(id)) => {
            (format!("Switch to context with id {}", id.get()), category, 30)
        }
        ContextCommand::AddWindowToContext(reference) => {
            let (name, order) = describe_context_ref(reference);
            (format!("Add the window to {name}"), category, 40 + order)
        }
        ContextCommand::MoveWindowToContext(reference) => {
            let (name, order) = describe_context_ref(reference);
            (format!("Move the window to {name}"), category, 60 + order)
        }
        ContextCommand::RemoveWindowFromContext => (
            "Remove the window from the active context".to_string(),
            category,
            80,
        ),
        ContextCommand::ToggleWindowPinned => ("Pin or unpin the window".to_string(), category, 81),
        ContextCommand::CreateContext(name) => (
            format!("Create context \"{name}\" from the windows on screen"),
            category,
            40,
        ),
        ContextCommand::AddWindow { context, .. } => {
            let (name, order) = describe_context_ref(context);
            (format!("Add a window to {name}"), category, 40 + order)
        }
        ContextCommand::MoveWindow { context, .. } => {
            let (name, order) = describe_context_ref(context);
            (format!("Move a window to {name}"), category, 60 + order)
        }
        ContextCommand::TogglePinned { .. } => ("Pin or unpin a window".to_string(), category, 81),
        ContextCommand::CreateContextFromWindows { name, .. } => (
            format!("Create context \"{name}\" from chosen windows"),
            category,
            40,
        ),
        ContextCommand::EditContext { context, .. } => {
            let (name, order) = describe_context_ref(context);
            (format!("Edit the windows of {name}"), category, 82 + order)
        }
        ContextCommand::RenameContext { context, name } => {
            let (context, order) = describe_context_ref(context);
            (format!("Rename {context} to \"{name}\""), category, 100 + order)
        }
        ContextCommand::SetContextNumber { context, number } => {
            let (context, order) = describe_context_ref(context);
            (
                format!("Give {context} the number {number}"),
                category,
                120 + order,
            )
        }
        ContextCommand::DeleteContext(context) => {
            let (context, order) = describe_context_ref(context);
            (format!("Delete {context}"), category, 140 + order)
        }
        ContextCommand::EditContextMembers { context, .. } => {
            let (context, order) = describe_context_ref(context);
            (format!("Change the windows of {context}"), category, 160 + order)
        }
        ContextCommand::RemoveRecord { context, record } => {
            let (context, order) = describe_context_ref(context);
            (
                format!("Forget member record {} of {context}", record.record),
                category,
                180 + order,
            )
        }
    }
}

/// The description and the offset in the sort order of a context that a
/// command names.
fn describe_context_ref(reference: &ContextRef) -> (String, u32) {
    match reference {
        ContextRef::Number(number) => (format!("context {number}"), u32::from(*number)),
        ContextRef::Name(name) => (format!("context \"{name}\""), 10),
        ContextRef::Id(id) => (format!("context with id {}", id.get()), 11),
    }
}

/// Get the description, category, and sort order for a LayoutCommand.
fn describe_layout_command(cmd: &LayoutCommand) -> (String, String, u32) {
    match cmd {
        LayoutCommand::MoveFocus(dir) => (
            format!("Focus {}", direction_name(dir)),
            "Focus".to_string(),
            direction_sort_order(dir),
        ),
        LayoutCommand::FocusNext => ("Focus next window".to_string(), "Focus".to_string(), 10),
        LayoutCommand::FocusPrev => ("Focus previous window".to_string(), "Focus".to_string(), 11),
        LayoutCommand::Ascend => ("Select parent container".to_string(), "Focus".to_string(), 20),
        LayoutCommand::Descend => ("Select child node".to_string(), "Focus".to_string(), 21),
        LayoutCommand::MoveNode(dir) => (
            format!("Move window {}", direction_name(dir)),
            "Move".to_string(),
            direction_sort_order(dir),
        ),
        LayoutCommand::Resize { direction, percent } => (
            format!("Resize {} by {}%", direction_name(direction), percent),
            "Resize".to_string(),
            10 + direction_sort_order(direction), // After SetSizeShare
        ),
        LayoutCommand::Split(orientation) => (
            format!("Split {}", orientation_name(orientation)),
            "Layout".to_string(),
            0,
        ),
        LayoutCommand::ToggleOrientation => {
            ("Toggle split orientation".to_string(), "Layout".to_string(), 1)
        }
        LayoutCommand::Group(orientation) => (
            format!(
                "Group {} ({})",
                orientation_name(orientation),
                group_mode_name(orientation)
            ),
            "Layout".to_string(),
            10,
        ),
        LayoutCommand::Ungroup => ("Ungroup container".to_string(), "Layout".to_string(), 11),
        LayoutCommand::ToggleFocusFloating => (
            "Toggle focus between tiled/floating".to_string(),
            "Floating".to_string(),
            0,
        ),
        LayoutCommand::ToggleWindowFloating => {
            ("Toggle window floating".to_string(), "Floating".to_string(), 1)
        }
        LayoutCommand::ToggleFullscreen => {
            ("Toggle fullscreen".to_string(), "Layout".to_string(), 20)
        }
        LayoutCommand::NextLayout => (
            "Switch to next saved layout".to_string(),
            "Layout".to_string(),
            30,
        ),
        LayoutCommand::PrevLayout => (
            "Switch to previous saved layout".to_string(),
            "Layout".to_string(),
            31,
        ),
        LayoutCommand::CycleColumnWidth => (
            "Cycle column width preset".to_string(),
            "Scroll Layout".to_string(),
            0,
        ),
        LayoutCommand::ChangeLayoutKind => (
            "Change layout mode (tree/scroll)".to_string(),
            "Scroll Layout".to_string(),
            1,
        ),
        LayoutCommand::ToggleColumnTabbed => (
            "Toggle column tabbed mode".to_string(),
            "Scroll Layout".to_string(),
            2,
        ),
        LayoutCommand::CleanUpSpace => ("Clean up space".to_string(), "System".to_string(), 5),
        LayoutCommand::SetSizeShare(share) => {
            // Use fractions for display: 1/2, 1/3, 1/4
            // Sort order: 1/2=0, 1/3=1, 1/4=2
            let (name, sort_order) = match share {
                SizeShare::Fraction(f) if *f == 0.5 => ("1/2".to_string(), 0),
                SizeShare::Fraction(f) if *f == 0.25 => ("1/4".to_string(), 2),
                SizeShare::Fraction(f) => (format!("{}%", (f * 100.0).round()), 5),
                SizeShare::Denominator { denominator: 3 } => ("1/3".to_string(), 1),
                SizeShare::Denominator { denominator } => {
                    (format!("1/{denominator}"), *denominator)
                }
            };
            (
                format!("Toggle window size {name}"),
                "Resize".to_string(),
                sort_order,
            )
        }
        LayoutCommand::ToggleSizeLock => (
            "Lock window at current size".to_string(),
            "Resize".to_string(),
            3, // After 1/4 (sort_order=2)
        ),
    }
}

fn direction_sort_order(dir: &Direction) -> u32 {
    match dir {
        Direction::Left => 0,
        Direction::Down => 1,
        Direction::Up => 2,
        Direction::Right => 3,
    }
}

fn direction_name(dir: &Direction) -> &'static str {
    match dir {
        Direction::Left => "left",
        Direction::Right => "right",
        Direction::Up => "up",
        Direction::Down => "down",
    }
}

fn orientation_name(orientation: &Orientation) -> &'static str {
    match orientation {
        Orientation::Horizontal => "horizontally",
        Orientation::Vertical => "vertically",
    }
}

fn group_mode_name(orientation: &Orientation) -> &'static str {
    match orientation {
        Orientation::Horizontal => "tabbed",
        Orientation::Vertical => "stacked",
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_preferences_json_roundtrip() {
        let prefs = PreferencesJson {
            status_icon_enable: true,
            animate: false,
            focus_follows_mouse: true,
            mouse_follows_focus: false,
            outer_gap: 10.0,
            inner_gap: 5.0,
            drag_drop_enable: true,
            drag_drop_live_preview: true,
            default_layout_kind: "tree".to_string(),
            contexts_enable: true,
            contexts_scope: Scope::PerScreen,
            window_rules: vec![WindowRuleJson {
                app_name: Some("Finder".to_string()),
                bundle_id: Some("com.apple.finder".to_string()),
                behavior: "float".to_string(),
                title_regex: None,
                title_substring: None,
                ax_role: None,
                ax_subrole: None,
            }],
            hotkeys: vec![HotkeyBindingJson {
                key: "⌥H".to_string(),
                command: r#"{"move_focus":"left"}"#.to_string(),
                description: "Focus left".to_string(),
                category: "Focus".to_string(),
                default_key: Some("⌥H".to_string()),
                sort_order: 0,
            }],
        };

        let json = serde_json::to_string(&prefs).unwrap();
        let decoded: PreferencesJson = serde_json::from_str(&json).unwrap();
        assert_eq!(prefs, decoded);
    }

    #[test]
    fn test_window_rule_json_conversion() {
        let rule = WindowRule {
            conditions: WindowRuleConditions {
                app_id: Some("com.apple.finder".to_string()),
                app_name: Some("Finder".to_string()),
                ..Default::default()
            },
            float: true,
        };

        let json = WindowRuleJson::from_rule(&rule);
        assert_eq!(json.bundle_id, Some("com.apple.finder".to_string()));
        assert_eq!(json.behavior, "float");

        let converted = json.to_rule().unwrap();
        assert_eq!(converted.conditions.app_id, rule.conditions.app_id);
        assert_eq!(converted.float, rule.float);
    }

    /// Window rules. Every condition survives the window's JSON, including
    /// the ones the App Rules pane doesn't show.
    #[test]
    fn window_rule_conditions_survive_the_preferences_json() {
        let rule = WindowRule {
            conditions: WindowRuleConditions {
                app_id: Some("com.apple.finder".to_string()),
                app_name: Some("Finder".to_string()),
                title_regex: Some("Picture-in-Picture".parse().unwrap()),
                title_substring: Some("Preferences".to_string()),
                ax_role: Some("AXWindow".to_string()),
                ax_subrole: Some("AXDialog".to_string()),
            },
            float: true,
        };

        let json = serde_json::to_string(&WindowRuleJson::from_rule(&rule)).unwrap();
        let decoded: WindowRuleJson = serde_json::from_str(&json).unwrap();
        assert_eq!(Some(rule), decoded.to_rule());
    }

    #[test]
    fn test_parse_hotkey_string() {
        // Simple modifier + letter
        let hk = parse_hotkey_string("⌥H").unwrap();
        assert_eq!(hk.to_string(), "Alt + KeyH");

        // Multiple modifiers
        let hk = parse_hotkey_string("⌥⇧H").unwrap();
        assert_eq!(hk.to_string(), "Alt + Shift + KeyH");

        // Ctrl + Alt + letter
        let hk = parse_hotkey_string("⌃⌥K").unwrap();
        assert_eq!(hk.to_string(), "Ctrl + Alt + KeyK");

        // Arrow keys
        let hk = parse_hotkey_string("⌥←").unwrap();
        assert_eq!(hk.to_string(), "Alt + ArrowLeft");

        // Special keys
        let hk = parse_hotkey_string("⌥Space").unwrap();
        assert_eq!(hk.to_string(), "Alt + Space");

        let hk = parse_hotkey_string("⌥\\").unwrap();
        assert_eq!(hk.to_string(), "Alt + Backslash");

        let hk = parse_hotkey_string("⌥/").unwrap();
        assert_eq!(hk.to_string(), "Alt + Slash");

        let hk = parse_hotkey_string("⌥=").unwrap();
        assert_eq!(hk.to_string(), "Alt + Equal");

        // Digits
        let hk = parse_hotkey_string("⌥⇧0").unwrap();
        assert_eq!(hk.to_string(), "Alt + Shift + Digit0");

        // The window shows the Command modifier as ⌘
        let hk = parse_hotkey_string("⌥⌘H").unwrap();
        assert_eq!(hk.to_string(), "Alt + Meta + KeyH");

        // Numpad keys and function keys keep their names
        let hk = parse_hotkey_string("⌥Numpad1").unwrap();
        assert_eq!(hk.to_string(), "Alt + Numpad1");
        let hk = parse_hotkey_string("⌥F5").unwrap();
        assert_eq!(hk.to_string(), "Alt + F5");

        // Enter is shown as ↩
        let hk = parse_hotkey_string("⌥↩").unwrap();
        assert_eq!(hk.to_string(), "Alt + Enter");

        // A key with no modifier is a hotkey too
        let hk = parse_hotkey_string("H").unwrap();
        assert_eq!(hk.to_string(), "KeyH");
    }

    /// Keys. A key without a modifier keeps its key through the window's
    /// JSON, config in and config out.
    #[test]
    fn a_hotkey_without_a_modifier_survives_the_preferences_round_trip() {
        let mut config = Config::default();
        config.keys = vec![(
            Hotkey::from_str("KeyH").unwrap(),
            WmCommand::Wm(WmCmd::ToggleGlobalEnabled),
        )];

        let json = serde_json::to_string(&PreferencesJson::from_config(&config)).unwrap();
        let prefs: PreferencesJson = serde_json::from_str(&json).unwrap();

        let row = prefs
            .hotkeys
            .iter()
            .find(|row| row.command.contains("toggle_global_enabled"))
            .expect("the binding is shown");
        assert_eq!("H", row.key);
        let applied = prefs.apply_to_config(&config);
        assert_eq!(1, applied.keys.len());
        assert_eq!(Hotkey::from_str("KeyH").unwrap(), applied.keys[0].0);
        assert_eq!(
            command_json(&WmCommand::Wm(WmCmd::ToggleGlobalEnabled)),
            command_json(&applied.keys[0].1)
        );
    }

    /// Key bindings. Two rows that name one hotkey resolve to the later row,
    /// and the running config gets one binding for it.
    #[test]
    fn two_rows_of_one_hotkey_resolve_to_the_later_row() {
        let hotkey = Hotkey::from_str("Alt + KeyZ").unwrap();
        let mut config = Config::default();
        config.keys = vec![
            (hotkey, WmCommand::Wm(WmCmd::ToggleGlobalEnabled)),
            (hotkey, WmCommand::Wm(WmCmd::ToggleSpaceActivated)),
        ];

        let json = serde_json::to_string(&PreferencesJson::from_config(&config)).unwrap();
        let prefs: PreferencesJson = serde_json::from_str(&json).unwrap();
        assert_eq!(2, prefs.hotkeys.len(), "the window shows both rows");

        let bindings = prefs.bindings();
        assert_eq!(1, bindings.len());
        assert_eq!(hotkey, bindings[0].0);
        assert_eq!(
            command_json(&WmCommand::Wm(WmCmd::ToggleSpaceActivated)),
            command_json(&bindings[0].1)
        );

        let applied = prefs.apply_to_config(&config);
        assert_eq!(1, applied.keys.len());
        assert_eq!(hotkey, applied.keys[0].0);
    }

    /// Keys. The config loader accepts these key names ([`KeyCode::name`]
    /// writes them), and every combination of the four modifiers.
    const KEY_NAMES: &[&str] = &[
        "Backquote",
        "Backslash",
        "BracketLeft",
        "BracketRight",
        "Comma",
        "Digit0",
        "Digit1",
        "Digit2",
        "Digit3",
        "Digit4",
        "Digit5",
        "Digit6",
        "Digit7",
        "Digit8",
        "Digit9",
        "Equal",
        "IntlBackslash",
        "IntlRo",
        "IntlYen",
        "KeyA",
        "KeyB",
        "KeyC",
        "KeyD",
        "KeyE",
        "KeyF",
        "KeyG",
        "KeyH",
        "KeyI",
        "KeyJ",
        "KeyK",
        "KeyL",
        "KeyM",
        "KeyN",
        "KeyO",
        "KeyP",
        "KeyQ",
        "KeyR",
        "KeyS",
        "KeyT",
        "KeyU",
        "KeyV",
        "KeyW",
        "KeyX",
        "KeyY",
        "KeyZ",
        "Minus",
        "Period",
        "Quote",
        "Semicolon",
        "Slash",
        "AltLeft",
        "AltRight",
        "Backspace",
        "CapsLock",
        "ContextMenu",
        "ControlLeft",
        "ControlRight",
        "Enter",
        "MetaLeft",
        "MetaRight",
        "ShiftLeft",
        "ShiftRight",
        "Space",
        "Tab",
        "Convert",
        "KanaMode",
        "Lang1",
        "Lang2",
        "Lang3",
        "Lang4",
        "Lang5",
        "NonConvert",
        "Delete",
        "End",
        "Help",
        "Home",
        "Insert",
        "PageDown",
        "PageUp",
        "ArrowDown",
        "ArrowLeft",
        "ArrowRight",
        "ArrowUp",
        "NumLock",
        "Numpad0",
        "Numpad1",
        "Numpad2",
        "Numpad3",
        "Numpad4",
        "Numpad5",
        "Numpad6",
        "Numpad7",
        "Numpad8",
        "Numpad9",
        "NumpadAdd",
        "NumpadBackspace",
        "NumpadClear",
        "NumpadClearEntry",
        "NumpadComma",
        "NumpadDecimal",
        "NumpadDivide",
        "NumpadEnter",
        "NumpadEqual",
        "NumpadHash",
        "NumpadMemoryAdd",
        "NumpadMemoryClear",
        "NumpadMemoryRecall",
        "NumpadMemoryStore",
        "NumpadMemorySubtract",
        "NumpadMultiply",
        "NumpadParenLeft",
        "NumpadParenRight",
        "NumpadStar",
        "NumpadSubtract",
        "Escape",
        "F1",
        "F2",
        "F3",
        "F4",
        "F5",
        "F6",
        "F7",
        "F8",
        "F9",
        "F10",
        "F11",
        "F12",
        "F13",
        "F14",
        "F15",
        "F16",
        "F17",
        "F18",
        "F19",
        "F20",
        "F21",
        "F22",
        "F23",
        "F24",
        "Fn",
        "FnLock",
        "PrintScreen",
        "ScrollLock",
        "Pause",
        "BrowserBack",
        "BrowserFavorites",
        "BrowserForward",
        "BrowserHome",
        "BrowserRefresh",
        "BrowserSearch",
        "BrowserStop",
        "Eject",
        "LaunchApp1",
        "LaunchApp2",
        "LaunchMail",
        "MediaPlayPause",
        "MediaSelect",
        "MediaStop",
        "MediaTrackNext",
        "MediaTrackPrevious",
        "Power",
        "Sleep",
        "AudioVolumeDown",
        "AudioVolumeMute",
        "AudioVolumeUp",
        "WakeUp",
        "Again",
        "Copy",
        "Cut",
        "Find",
        "Open",
        "Paste",
        "Props",
        "Select",
        "Undo",
        "Gamepad0",
        "Gamepad1",
        "Gamepad2",
        "Gamepad3",
        "Gamepad4",
        "Gamepad5",
        "Gamepad6",
        "Gamepad7",
        "Gamepad8",
        "Gamepad9",
        "Gamepad10",
        "Gamepad11",
        "Gamepad12",
        "Gamepad13",
        "Gamepad14",
        "Gamepad15",
        "Gamepad16",
        "Gamepad17",
        "Gamepad18",
        "Gamepad19",
        "BrightnessDown",
        "BrightnessUp",
        "DisplayToggleIntExt",
        "KeyboardLayoutSelect",
        "LaunchAssistant",
        "LaunchControlPanel",
        "LaunchScreenSaver",
        "MailForward",
        "MailReply",
        "MailSend",
        "MediaFastForward",
        "MediaPlay",
        "MediaPause",
        "MediaRecord",
        "MediaRewind",
        "MicrophoneMuteToggle",
        "PrivacyScreenToggle",
        "SelectTask",
        "ShowAllWindows",
        "ZoomToggle",
    ];

    /// Keys. Config -> window -> config is identity for every key the loader
    /// accepts, with every modifier combination and with none.
    #[test]
    fn every_hotkey_string_survives_config_window_config() {
        for bits in 0..16 {
            let mut modifiers = Modifiers::empty();
            if bits & 1 != 0 {
                modifiers |= Modifiers::SHIFT;
            }
            if bits & 2 != 0 {
                modifiers |= Modifiers::CONTROL;
            }
            if bits & 4 != 0 {
                modifiers |= Modifiers::ALT;
            }
            if bits & 8 != 0 {
                modifiers |= Modifiers::META;
            }
            for name in KEY_NAMES {
                let text = if modifiers.is_empty() {
                    (*name).to_string()
                } else {
                    format!("{modifiers} + {name}")
                };
                let hotkey = Hotkey::from_str(&text)
                    .unwrap_or_else(|()| panic!("the loader rejects its own key name: {text}"));
                let shown = format_hotkey(&hotkey);
                assert_eq!(
                    Some(hotkey),
                    parse_hotkey_string(&shown),
                    "{text} is shown as {shown}"
                );
            }
        }
    }

    /// Key bindings. Context bindings survive the round trip through the
    /// preferences JSON, each with its own command.
    #[test]
    fn context_bindings_survive_the_preferences_round_trip() {
        let id = serde_json::from_value(serde_json::json!(7)).unwrap();
        let bindings = [
            ("Ctrl + Alt + Digit0", ContextCommand::ShowEverything),
            (
                "Ctrl + Alt + Digit1",
                ContextCommand::SwitchContext(ContextRef::Number(1)),
            ),
            (
                "Ctrl + Alt + Digit2",
                ContextCommand::SwitchContext(ContextRef::Number(2)),
            ),
            (
                "Ctrl + Alt + KeyC",
                ContextCommand::SwitchContext(ContextRef::Name("Comms".into())),
            ),
            (
                "Ctrl + Alt + KeyI",
                ContextCommand::SwitchContext(ContextRef::Id(id)),
            ),
            ("Ctrl + Alt + Tab", ContextCommand::PreviousContext),
            (
                "Ctrl + Alt + KeyA",
                ContextCommand::AddWindowToContext(ContextRef::Number(2)),
            ),
            (
                "Ctrl + Alt + KeyM",
                ContextCommand::MoveWindowToContext(ContextRef::Name("Comms".into())),
            ),
            ("Ctrl + Alt + KeyR", ContextCommand::RemoveWindowFromContext),
            ("Ctrl + Alt + KeyP", ContextCommand::ToggleWindowPinned),
        ];
        let mut config = Config::default();
        config.keys = bindings
            .iter()
            .map(|(key, cmd)| {
                let cmd = WmCommand::ReactorCommand(ReactorCommand::Context(cmd.clone()));
                (Hotkey::from_str(key).unwrap(), cmd)
            })
            .collect();

        let json = serde_json::to_string(&PreferencesJson::from_config(&config)).unwrap();
        let prefs: PreferencesJson = serde_json::from_str(&json).unwrap();
        let applied = prefs.apply_to_config(&config);

        let mut commands: Vec<&str> = prefs.hotkeys.iter().map(|hk| hk.command.as_str()).collect();
        commands.sort();
        assert_eq!(
            vec![
                r#""previous_context""#,
                r#""remove_window_from_context""#,
                r#""show_everything""#,
                r#""toggle_window_pinned""#,
                r#"{"add_window_to_context":2}"#,
                r#"{"move_window_to_context":"Comms"}"#,
                r#"{"switch_context":"Comms"}"#,
                r#"{"switch_context":1}"#,
                r#"{"switch_context":2}"#,
                r#"{"switch_context":{"id":7}}"#,
            ],
            commands
        );
        let mut keys: Vec<(String, ContextCommand)> = applied
            .keys
            .iter()
            .map(|(hotkey, cmd)| match cmd {
                WmCommand::ReactorCommand(ReactorCommand::Context(cmd)) => {
                    (hotkey.to_string(), cmd.clone())
                }
                other => panic!("{other:?}"),
            })
            .collect();
        keys.sort_by(|a, b| a.0.cmp(&b.0));
        let mut expected: Vec<(String, ContextCommand)> =
            bindings.iter().map(|(key, cmd)| (key.to_string(), cmd.clone())).collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(expected, keys);
    }

    /// Bindings whose former command ids collided keep separate rows and
    /// round-trip to their original commands. The Preferences payload carries
    /// each command itself, so a name with a suffix cannot overwrite a
    /// duplicate of its unsuffixed name.
    #[test]
    fn context_bindings_with_colliding_former_command_ids_survive_the_preferences_round_trip() {
        let bindings = [
            (
                "Ctrl + Alt + KeyA",
                ContextCommand::SwitchContext(ContextRef::Name("x".into())),
            ),
            (
                "Ctrl + Alt + KeyB",
                ContextCommand::SwitchContext(ContextRef::Name("x".into())),
            ),
            (
                "Ctrl + Alt + KeyC",
                ContextCommand::SwitchContext(ContextRef::Name("x#2".into())),
            ),
        ];
        let mut config = Config::default();
        config.keys = bindings
            .iter()
            .map(|(key, command)| {
                (
                    Hotkey::from_str(key).unwrap(),
                    WmCommand::ReactorCommand(ReactorCommand::Context(command.clone())),
                )
            })
            .collect();

        let prefs = PreferencesJson::from_config(&config);
        let mut commands: Vec<&str> =
            prefs.hotkeys.iter().map(|binding| binding.command.as_str()).collect();
        commands.sort();
        assert_eq!(
            vec![
                r#"{"switch_context":"x"}"#,
                r#"{"switch_context":"x"}"#,
                r#"{"switch_context":"x#2"}"#,
            ],
            commands
        );

        let mut actual: Vec<(String, ContextCommand)> = prefs
            .apply_to_config(&config)
            .keys
            .into_iter()
            .map(|(hotkey, command)| match command {
                WmCommand::ReactorCommand(ReactorCommand::Context(command)) => {
                    (hotkey.to_string(), command)
                }
                other => panic!("{other:?}"),
            })
            .collect();
        actual.sort_by(|left, right| left.0.cmp(&right.0));
        let mut expected: Vec<(String, ContextCommand)> = bindings
            .iter()
            .map(|(key, command)| (key.to_string(), command.clone()))
            .collect();
        expected.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(expected, actual);
    }

    /// The Preferences switch shows and sets
    /// `settings.experimental.contexts.enable` and leaves the other
    /// experimental settings alone.
    #[test]
    fn the_contexts_switch_follows_the_contexts_flag() {
        let mut config = Config::default();
        config.settings.experimental.contexts.enable = true;
        config.settings.experimental.scroll.enable = true;

        let json = serde_json::to_value(PreferencesJson::from_config(&config)).unwrap();
        assert_eq!(serde_json::json!(true), json["contextsEnable"]);

        let mut prefs: PreferencesJson = serde_json::from_value(json).unwrap();
        prefs.contexts_enable = false;
        let applied = prefs.apply_to_config(&config);
        assert!(!applied.settings.experimental.contexts.enable);
        assert!(applied.settings.experimental.scroll.enable);

        prefs.contexts_enable = true;
        assert!(prefs.apply_to_config(&applied).settings.experimental.contexts.enable);
    }

    #[test]
    fn the_scope_picker_round_trips_through_preferences_json() {
        let mut config = Config::default();
        config.settings.experimental.contexts.scope = Scope::PerScreen;

        let json = serde_json::to_value(PreferencesJson::from_config(&config)).unwrap();
        assert_eq!(serde_json::json!("per_screen"), json["contextsScope"]);

        let mut prefs: PreferencesJson = serde_json::from_value(json.clone()).unwrap();
        prefs.contexts_scope = Scope::Global;
        assert_eq!(
            Scope::Global,
            prefs.apply_to_config(&config).settings.experimental.contexts.scope
        );

        let mut invalid = json;
        invalid["contextsScope"] = serde_json::json!("sometimes");
        assert!(serde_json::from_value::<PreferencesJson>(invalid).is_err());
    }

    #[test]
    fn test_format_hotkey_roundtrip() {
        // Parse a hotkey, format it, parse it back
        let original = Hotkey::from_str("Alt + Shift + KeyJ").unwrap();
        let formatted = format_hotkey(&original);
        assert_eq!(formatted, "⌥⇧J");

        let parsed_back = parse_hotkey_string(&formatted).unwrap();
        assert_eq!(parsed_back.to_string(), original.to_string());
    }
}

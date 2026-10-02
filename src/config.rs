// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

// Design note: Config sub-types should generally not implement Default unless
// delegating to Config::default(), which parses sugarglider.default.toml. A manual
// Default impl with hardcoded values can silently diverge from the TOML file,
// causing different behavior in code paths that don't load the config file
// (tests, first run, deserialization of saved state).

#[macro_use]
mod partial;
use std::fs::File;
use std::io::Read;
use std::ops::{Deref, Range};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use indexmap::IndexMap;
use livesplit_hotkey::Hotkey;
use macro_rules_attribute::derive;
use partial::{PartialConfig, ValidationError};
use regex::{Regex, RegexBuilder};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::actor::wm_controller::WmCommand;
use crate::model::LayoutKind;
use crate::model::contexts::Scope;
use crate::ui::preferences_json::{WindowRuleJson, command_json};

pub fn data_dir() -> PathBuf {
    dirs::home_dir().unwrap().join(".glide")
}

pub fn restore_file() -> PathBuf {
    data_dir().join("layout.ron")
}

pub fn parked_journal_file() -> PathBuf {
    data_dir().join("parked.json")
}

pub fn contexts_file() -> PathBuf {
    data_dir().join("contexts.json")
}

pub fn context_commands_dir() -> PathBuf {
    dirs::home_dir()
        .expect("Could not determine home directory")
        .join(".config/raycast/script-commands/sugarglider-contexts")
}

pub fn config_path() -> PathBuf {
    let try_paths = default_config_paths();
    for path in &try_paths {
        if path.try_exists().unwrap_or(false) {
            return path.clone();
        }
    }
    try_paths[0].clone()
}

fn default_config_paths() -> Vec<PathBuf> {
    let home = dirs::home_dir().expect("Could not determine home directory");
    let xdg_path = home.join(".config/glide/glide.toml");
    let legacy_path = home.join(".glide.toml");

    let mut paths = vec![xdg_path.clone()];
    if legacy_path != xdg_path {
        paths.push(legacy_path);
    }
    paths
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    pub settings: Settings,
    pub window_rules: Vec<WindowRule>,
    pub keys: Vec<(Hotkey, WmCommand)>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[serde(default)]
struct ConfigPartial {
    settings: SettingsPartial,
    window_rules: Option<Vec<WindowRule>>,
    /// The `[keys]` table in the order the file writes it, so that two
    /// spellings of one hotkey can resolve to the last one.
    keys: Option<IndexMap<String, WmCommandOrDisable>>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum WmCommandOrDisable {
    WmCommand(WmCommand),
    Disable(Disabled),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Disabled {
    Disable,
}

#[derive(PartialConfig!)]
#[derive_args(SettingsPartial)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub animate: bool,
    pub default_disable: bool,
    pub mouse_follows_focus: bool,
    pub mouse_hides_on_focus: bool,
    pub focus_follows_mouse: bool,
    pub outer_gap: f64,
    pub inner_gap: f64,
    pub default_keys: bool,
    pub default_layout_kind: LayoutKind,
    #[derive_args(GroupBarsPartial)]
    pub group_bars: GroupBars,
    #[derive_args(SizeShareConfigPartial)]
    pub size_share: SizeShareConfig,
    #[derive_args(StatusIconPartial)]
    pub status_icon: StatusIcon,
    #[derive_args(DragDropConfigPartial)]
    pub drag_drop: DragDropConfig,
    #[derive_args(ExperimentalPartial)]
    pub experimental: Experimental,
}

/// A [`Regex`] sourced from config. Deserializing compiles (and thus
/// validates) the pattern, so an invalid regex surfaces as a config error at
/// parse time rather than being silently ignored later. Serializes back to the
/// original pattern string, and compares by pattern.
#[derive(Debug, Clone)]
pub struct ConfigRegex(Regex);

impl Deref for ConfigRegex {
    type Target = Regex;
    fn deref(&self) -> &Regex {
        &self.0
    }
}

impl FromStr for ConfigRegex {
    type Err = regex::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RegexBuilder::new(s).case_insensitive(true).build().map(ConfigRegex)
    }
}

impl PartialEq for ConfigRegex {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl<'de> Deserialize<'de> for ConfigRegex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let pattern = String::deserialize(deserializer)?;
        pattern.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for ConfigRegex {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

/// A rule that overrides how a window is managed when it is first observed,
/// based on properties of the window and its application.
///
/// Conditions are nested under `if`; all specified conditions must match
/// (logical AND) for the rule to apply. Rules are evaluated in order and the
/// first matching rule wins. See `window_rules` in the configuration.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct WindowRule {
    /// Conditions a window must satisfy for this rule to apply.
    #[serde(rename = "if", default)]
    pub conditions: WindowRuleConditions,
    /// Whether matching windows should float (`true`) or tile (`false`).
    pub float: bool,
}

/// Conditions matched against a window and its application. All specified
/// conditions must match (logical AND); omitted conditions are ignored, so an
/// empty set of conditions matches every window. All conditions match
/// case-insensitively.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct WindowRuleConditions {
    /// Application bundle identifier, matched exactly (e.g. "com.apple.Safari").
    pub app_id: Option<String>,
    /// Substring match on the application's localized name.
    pub app_name: Option<String>,
    /// Regex matched against the window title. Must be a valid regex or the
    /// config is rejected.
    pub title_regex: Option<ConfigRegex>,
    /// Literal substring match on the window title.
    pub title_substring: Option<String>,
    /// Match for the macOS Accessibility AXRole (e.g. "AXWindow").
    pub ax_role: Option<String>,
    /// Match for the macOS Accessibility AXSubrole (e.g. "AXDialog").
    pub ax_subrole: Option<String>,
}

#[derive(PartialConfig!)]
#[derive_args(ExperimentalPartial)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct Experimental {
    #[derive_args(StatusIconExperimentalPartial)]
    pub status_icon: StatusIconExperimental,
    #[derive_args(ScrollConfigPartial)]
    pub scroll: ScrollConfig,
    #[derive_args(ContextsConfigPartial)]
    pub contexts: ContextsConfig,
}

#[derive(PartialConfig!)]
#[derive_args(ContextsConfigPartial)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ContextsConfig {
    /// Named window sets that the user switches between.
    pub enable: bool,
    /// "global": a switch changes every screen. "per_screen": only the
    /// focused screen, and the target's members come along (R7, R8, R11).
    pub scope: Scope,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum NewWindowPlacement {
    NewColumn,
    SameColumn,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum CenterMode {
    #[default]
    Never,
    Always,
    OnOverflow,
}

#[derive(PartialConfig!)]
#[derive_args(ScrollConfigPartial)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ScrollConfig {
    pub enable: bool,
    pub center_focused_column: CenterMode,
    pub visible_columns: u32,
    pub column_width_presets: Vec<f64>,
    pub new_window_in_column: NewWindowPlacement,
    pub scroll_sensitivity: f64,
    pub invert_scroll_direction: bool,
    pub infinite_loop: bool,
    pub single_column_aspect_ratio: String,
}

#[derive(PartialConfig!)]
#[derive_args(DragDropConfigPartial)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct DragDropConfig {
    /// Enable drag-to-rearrange windows.
    pub enable: bool,
    /// Show live preview of window positions while dragging.
    /// When true, other windows move in real-time to show where they'll land.
    /// When false, windows only move after releasing the mouse button.
    pub live_preview: bool,
    /// Minimum drag distance in pixels before showing drop zones.
    pub drag_threshold: f64,
    /// Width/height of edge zones as a ratio of window dimension (0.0-0.5).
    pub edge_zone_ratio: f64,
    /// Time in milliseconds to hover in an edge zone before split activates.
    pub split_dwell_ms: u64,
}

impl Default for DragDropConfig {
    fn default() -> Self {
        Config::default().settings.drag_drop
    }
}

impl DragDropConfig {
    pub fn validated(mut self) -> Self {
        self.drag_threshold = self.drag_threshold.clamp(1.0, 100.0);
        self.edge_zone_ratio = self.edge_zone_ratio.clamp(0.05, 0.4);
        self.split_dwell_ms = self.split_dwell_ms.clamp(0, 2000);
        self
    }
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Config::default().settings.experimental.scroll
    }
}

impl ScrollConfig {
    pub fn validated(mut self) -> Self {
        self.visible_columns = self.visible_columns.clamp(1, 5);
        self.scroll_sensitivity = self.scroll_sensitivity.clamp(0.0, 100.0);
        self.column_width_presets.retain(|&p| p > 0.0 && p <= 1.0);
        self
    }

    pub fn aspect_ratio(&self) -> Option<AspectRatio> {
        if self.single_column_aspect_ratio.is_empty() {
            return None;
        }
        AspectRatio::from_str(&self.single_column_aspect_ratio).ok()
    }
}

#[derive(Debug, PartialEq, Clone, Copy, Serialize)]
pub struct AspectRatio {
    pub width: f64,
    pub height: f64,
}

impl FromStr for AspectRatio {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (w, h) =
            s.split_once(':').ok_or_else(|| format!("expected 'W:H' format, got {s:?}"))?;
        let width: f64 = w.trim().parse().map_err(|_| format!("invalid width: {w:?}"))?;
        let height: f64 = h.trim().parse().map_err(|_| format!("invalid height: {h:?}"))?;
        if width <= 0.0 || height <= 0.0 {
            return Err("aspect ratio values must be positive".into());
        }
        Ok(AspectRatio { width, height })
    }
}

impl<'de> Deserialize<'de> for AspectRatio {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        AspectRatio::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(PartialConfig!)]
#[derive_args(StatusIconPartial)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct StatusIcon {
    pub enable: bool,
}

#[derive(PartialConfig!)]
#[derive_args(StatusIconExperimentalPartial)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct StatusIconExperimental {
    pub space_index: bool,
    pub color: bool,

    #[deprecated = "Ignored; kept for compatibility."]
    pub enable: bool,
}

#[derive(PartialConfig!)]
#[derive_args(GroupBarsPartial)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct GroupBars {
    pub enable: bool,
    pub thickness: f64,
    pub horizontal_placement: HorizontalPlacement,
    pub vertical_placement: VerticalPlacement,
}

/// How to handle size share locks that would cover more than the screen.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum SizeShareOverflow {
    /// Refuse the new lock, keeping every existing lock exact.
    #[default]
    Reject,
    /// Accept the new lock and scale all locks down until they fit.
    Squeeze,
}

#[derive(PartialConfig!)]
#[derive_args(SizeShareConfigPartial)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct SizeShareConfig {
    pub overflow: SizeShareOverflow,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum HorizontalPlacement {
    Top,
    Bottom,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum VerticalPlacement {
    Left,
    Right,
}

impl GroupBars {
    /// Get the indicator thickness for layout space reservation
    pub fn indicator_thickness(&self) -> f64 {
        if self.enable { self.thickness } else { 0.0 }
    }
}

impl ConfigPartial {
    fn default() -> Self {
        toml::from_str(include_str!("../sugarglider.default.toml")).unwrap()
    }

    fn validate(self) -> Result<Config, SpannedError> {
        let mut keys: Vec<(Hotkey, WmCommand)> = Vec::new();
        for (key, cmd) in self.keys.unwrap_or_default() {
            let cmd = match cmd {
                WmCommandOrDisable::WmCommand(wm_command) => wm_command,
                WmCommandOrDisable::Disable(_) => continue,
            };
            let Ok(hotkey) = Hotkey::from_str(&key) else {
                return Err(SpannedError {
                    message: format!("Could not parse hotkey: {key}"),
                    span: None,
                });
            };
            // "Alt + T" and "Alt + KeyT" name the same hotkey; the last
            // entry wins, as in a TOML table.
            keys.retain(|(bound, _)| *bound != hotkey);
            keys.push((hotkey, cmd));
        }
        Ok(Config {
            settings: self.settings.validate()?,
            window_rules: self.window_rules.unwrap_or_default(),
            keys,
        })
    }

    fn merge(low: Self, high: Self) -> Self {
        let include_default_keys = high.keys.is_none()
            || high.settings.default_keys.unwrap_or(Config::default().settings.default_keys);
        let mut keys = if include_default_keys {
            low.keys.unwrap_or_default()
        } else {
            Default::default()
        };
        for (key, cmd) in high.keys.unwrap_or_default() {
            // "Alt + T" and "Alt + KeyT" name the same hotkey.
            if let Ok(hotkey) = Hotkey::from_str(&key) {
                keys.retain(|low_key, _| Hotkey::from_str(low_key) != Ok(hotkey));
            }
            keys.insert(key, cmd);
        }
        Self {
            settings: SettingsPartial::merge(low.settings, high.settings),
            window_rules: high.window_rules.or(low.window_rules),
            keys: Some(keys),
        }
    }
}

impl Config {
    pub fn load(custom_path: Option<&Path>) -> anyhow::Result<Config> {
        let mut buf = String::new();
        let (mut file, path) = match custom_path {
            Some(path) => (File::open(path)?, path.to_path_buf()),
            None => {
                let mut selected: Option<(File, PathBuf)> = None;
                for path in default_config_paths() {
                    match File::open(&path) {
                        Ok(file) => {
                            selected = Some((file, path));
                            break;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
                match selected {
                    Some(pair) => pair,
                    None => return Ok(Config::default()),
                }
            }
        };
        file.read_to_string(&mut buf)?;
        Self::parse(&buf).map_err(|e| {
            let renderer = annotate_snippets::Renderer::styled();
            anyhow::anyhow!("{}", format_toml_error(e, &buf, &path, renderer))
        })
    }

    pub fn default() -> Config {
        ConfigPartial::default().validate().unwrap()
    }

    fn parse(buf: &str) -> Result<Self, SpannedError> {
        let c: ConfigPartial = toml::from_str(buf)?;
        let defaults = ConfigPartial::default();
        ConfigPartial::merge(defaults, c).validate()
    }
}

fn format_toml_error(
    error: SpannedError,
    input: &str,
    path: &Path,
    renderer: annotate_snippets::Renderer,
) -> String {
    use annotate_snippets::{AnnotationKind, Level, Snippet};

    let message = error.message;
    let Some(span) = error.span else {
        return format!("could not parse config: {}", message);
    };

    let snippet = Snippet::source(input)
        .path(path.to_string_lossy())
        .annotation(AnnotationKind::Primary.span(span.start..span.end).label(message));

    let report = Level::ERROR.primary_title("could not parse config").element(snippet);

    format!("{}", renderer.render(&[report]))
}

#[derive(Debug)]
struct SpannedError {
    message: String,
    span: Option<Range<usize>>,
}

impl From<toml::de::Error> for SpannedError {
    fn from(e: toml::de::Error) -> Self {
        Self {
            message: e.message().to_owned(),
            span: e.span(),
        }
    }
}

impl From<ValidationError> for SpannedError {
    fn from(e: ValidationError) -> Self {
        Self {
            message: format!("{e}"),
            span: None, // TODO
        }
    }
}

/// Write preferences to the config file.
///
/// This function reads the existing config file (if present), updates the
/// relevant settings, and writes it back. It preserves user comments and
/// formatting where possible. A config file with an error is left unchanged,
/// and the error says what is wrong.
pub fn write_preferences_to_file(
    prefs: &crate::ui::preferences_json::PreferencesJson,
) -> anyhow::Result<PathBuf> {
    let path = config_path();
    write_preferences_to_path(prefs, &path)?;
    Ok(path)
}

/// Write preferences to the config file at `path`, as
/// [`write_preferences_to_file`] does.
fn write_preferences_to_path(
    prefs: &crate::ui::preferences_json::PreferencesJson,
    path: &Path,
) -> anyhow::Result<()> {
    use std::fs;
    use std::io::Write;

    use annotate_snippets::Renderer;
    use toml_edit::{DocumentMut, value};

    let existing = match fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let has_error = |error: String| {
        anyhow::anyhow!(
            "{} has an error, so it was not changed.\n\n{error}",
            path.display()
        )
    };
    // toml_edit reads TOML 1.0 and the config reader TOML 1.1, so a file can
    // pass one and fail the other.
    let current_config = Config::parse(&existing)
        .map_err(|e| has_error(format_toml_error(e, &existing, path, Renderer::plain())))?;
    let mut doc = existing
        .parse::<DocumentMut>()
        .map_err(|e| has_error(e.to_string().trim_end().to_owned()))?;

    // Ensure [settings] table exists
    if !doc.contains_key("settings") {
        doc["settings"] = toml_edit::Item::Table(toml_edit::Table::new());
    }

    // Update settings
    doc["settings"]["animate"] = value(prefs.animate);
    doc["settings"]["focus_follows_mouse"] = value(prefs.focus_follows_mouse);
    doc["settings"]["mouse_follows_focus"] = value(prefs.mouse_follows_focus);
    doc["settings"]["outer_gap"] = value(prefs.outer_gap);
    doc["settings"]["inner_gap"] = value(prefs.inner_gap);
    doc["settings"]["default_layout_kind"] = value(&prefs.default_layout_kind);

    // Ensure [settings.status_icon] table exists
    if !doc["settings"].as_table().map_or(false, |t| t.contains_key("status_icon")) {
        doc["settings"]["status_icon"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["settings"]["status_icon"]["enable"] = value(prefs.status_icon_enable);

    // Ensure [settings.drag_drop] table exists. Indexing a missing key would
    // create an inline table.
    if let Some(settings) = doc["settings"].as_table_mut() {
        let mut drag_drop = toml_edit::Table::new();
        drag_drop.set_dotted(settings.is_dotted());
        settings.entry("drag_drop").or_insert(toml_edit::Item::Table(drag_drop));
    }
    doc["settings"]["drag_drop"]["enable"] = value(prefs.drag_drop_enable);
    doc["settings"]["drag_drop"]["live_preview"] = value(prefs.drag_drop_live_preview);

    // Ensure [settings.experimental.contexts] table exists. Indexing a missing
    // key would create an inline table.
    if let Some(settings) = doc["settings"].as_table_mut() {
        let experimental = settings.entry("experimental").or_insert_with(|| {
            let mut experimental = toml_edit::Table::new();
            experimental.set_implicit(true);
            toml_edit::Item::Table(experimental)
        });
        if let Some(experimental) = experimental.as_table_mut() {
            let mut contexts = toml_edit::Table::new();
            contexts.set_dotted(experimental.is_dotted());
            experimental.entry("contexts").or_insert(toml_edit::Item::Table(contexts));
        }
    }
    doc["settings"]["experimental"]["contexts"]["enable"] = value(prefs.contexts_enable);
    let scope = match prefs.contexts_scope {
        Scope::Global => "global",
        Scope::PerScreen => "per_screen",
    };
    doc["settings"]["experimental"]["contexts"]["scope"] = value(scope);

    // Update window_rules only when the window changed them. The window
    // carries every condition through, but the comments and the rest of a
    // rule's table are not in the JSON, so an unchanged list leaves the
    // document's rules as they are.
    let window_rules: Vec<WindowRule> =
        prefs.window_rules.iter().filter_map(WindowRuleJson::to_rule).collect();
    if window_rules != current_config.window_rules {
        let mut rules_array = toml_edit::ArrayOfTables::new();
        for rule in &window_rules {
            let mut table = toml_edit::Table::new();

            // Build the 'if' conditions table
            let mut conditions = toml_edit::Table::new();
            // An empty string is a condition too, so it is written as it
            // stands: dropping it would widen the rule.
            if let Some(ref app_id) = rule.conditions.app_id {
                conditions["app_id"] = value(app_id);
            }
            if let Some(ref app_name) = rule.conditions.app_name {
                conditions["app_name"] = value(app_name);
            }
            if let Some(ref title_regex) = rule.conditions.title_regex {
                conditions["title_regex"] = value(title_regex.as_str());
            }
            if let Some(ref title_substring) = rule.conditions.title_substring {
                conditions["title_substring"] = value(title_substring);
            }
            if let Some(ref ax_role) = rule.conditions.ax_role {
                conditions["ax_role"] = value(ax_role);
            }
            if let Some(ref ax_subrole) = rule.conditions.ax_subrole {
                conditions["ax_subrole"] = value(ax_subrole);
            }
            if !conditions.is_empty() {
                table["if"] = toml_edit::Item::Table(conditions);
            }

            table["float"] = value(rule.float);
            rules_array.push(table);
        }

        if !rules_array.is_empty() {
            doc["window_rules"] = toml_edit::Item::ArrayOfTables(rules_array);
        } else if doc.contains_key("window_rules") {
            doc.remove("window_rules");
        }
    }

    // Update [keys] section, only when a binding changed. No bindings at all
    // means that the window has none to show, not that the user removed them.
    let bindings = prefs.bindings();
    if !bindings.is_empty() && sorted_bindings(&bindings) != sorted_bindings(&current_config.keys) {
        let file_keys = keys_in_file(&existing)?;
        let default_keys = current_config.settings.default_keys;
        let entries = keys_entries(&bindings, default_keys, &file_keys);
        set_keys_table(&mut doc, &file_keys, entries);
    }

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Write atomically using a temp file
    let tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    write!(tmp.as_file(), "{}", doc)?;
    tmp.persist(path)?;

    Ok(())
}

/// The entries under `[keys]` in a config file, by the key as the file
/// spells it, each with its value as JSON.
fn keys_in_file(file: &str) -> anyhow::Result<FxHashMap<String, serde_json::Value>> {
    let keys = toml::from_str::<ConfigPartial>(file)?.keys.unwrap_or_default();
    let keys = keys.into_iter().map(|(key, entry)| Ok((key, serde_json::to_value(entry)?)));
    keys.collect()
}

/// Key bindings as sorted pairs of a key and a command, for comparison.
fn sorted_bindings(bindings: &[(Hotkey, WmCommand)]) -> Vec<(String, String)> {
    let mut bindings: Vec<_> = bindings
        .iter()
        .map(|(hotkey, cmd)| (hotkey.to_string(), command_json(cmd).to_string()))
        .collect();
    bindings.sort();
    bindings
}

/// The `[keys]` entries that bind `bindings`, as JSON. With `default_keys`,
/// the default bindings are left out, and each default key that isn't bound
/// is disabled. A key that `file_keys` disables stays disabled unless it is
/// bound.
fn keys_entries(
    bindings: &[(Hotkey, WmCommand)],
    default_keys: bool,
    file_keys: &FxHashMap<String, serde_json::Value>,
) -> Vec<(Hotkey, serde_json::Value)> {
    let disable = serde_json::to_value(Disabled::Disable).unwrap();
    let is_bound = |hotkey: &Hotkey| bindings.iter().any(|(bound, _)| bound == hotkey);
    let defaults: Vec<(Hotkey, serde_json::Value)> = if default_keys {
        let defaults = Config::default().keys;
        defaults.iter().map(|(hotkey, cmd)| (*hotkey, command_json(cmd))).collect()
    } else {
        Vec::new()
    };

    let mut entries: Vec<(Hotkey, serde_json::Value)> = Vec::new();
    for (hotkey, cmd) in bindings {
        // A key bound twice keeps the last binding, as in a TOML table.
        entries.retain(|(entry, _)| entry != hotkey);
        let binding = (*hotkey, command_json(cmd));
        if !defaults.contains(&binding) {
            entries.push(binding);
        }
    }
    let disabled_in_file = file_keys
        .iter()
        .filter(|(_, value)| **value == disable)
        .filter_map(|(key, _)| Hotkey::from_str(key).ok());
    for hotkey in disabled_in_file.chain(defaults.iter().map(|(hotkey, _)| *hotkey)) {
        if !is_bound(&hotkey) && !entries.iter().any(|(entry, _)| *entry == hotkey) {
            entries.push((hotkey, disable.clone()));
        }
    }
    entries
}

/// Makes the `[keys]` table hold exactly `entries`. An entry that the table
/// already holds keeps its spelling and comments.
fn set_keys_table(
    doc: &mut toml_edit::DocumentMut,
    file_keys: &FxHashMap<String, serde_json::Value>,
    mut entries: Vec<(Hotkey, serde_json::Value)>,
) {
    if !doc.contains_key("keys") && entries.is_empty() {
        return;
    }
    let keys = doc.entry("keys").or_insert(toml_edit::table());
    let Some(table) = keys.as_table_like_mut() else {
        return;
    };
    let spellings: Vec<String> = table.iter().map(|(key, _)| key.to_owned()).collect();
    for key in spellings {
        let hotkey = Hotkey::from_str(&key).ok();
        let Some(i) = entries.iter().position(|(entry, _)| Some(*entry) == hotkey) else {
            table.remove(&key);
            continue;
        };
        let (_, value) = entries.remove(i);
        if file_keys.get(&key) != Some(&value) {
            let item = table.get_mut(&key).unwrap();
            let mut new_item = json_to_toml_value(&value);
            if let (Some(old), Some(new)) = (item.as_value(), new_item.as_value_mut()) {
                *new.decor_mut() = old.decor().clone();
            }
            *item = new_item;
        }
    }
    for (hotkey, value) in entries {
        table.insert(&hotkey.to_string(), json_to_toml_value(&value));
    }
}

/// Convert a JSON value to a TOML value.
fn json_to_toml_value(json: &serde_json::Value) -> toml_edit::Item {
    use toml_edit::{Item, Value, value};

    match json {
        serde_json::Value::Null => Item::None,
        serde_json::Value::Bool(b) => value(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                value(i)
            } else if let Some(f) = n.as_f64() {
                value(f)
            } else {
                Item::None
            }
        }
        serde_json::Value::String(s) => value(s.as_str()),
        serde_json::Value::Array(arr) => {
            let mut toml_arr = toml_edit::Array::new();
            for item in arr {
                if let Item::Value(v) = json_to_toml_value(item) {
                    toml_arr.push(v);
                }
            }
            Item::Value(Value::Array(toml_arr))
        }
        serde_json::Value::Object(obj) => {
            let mut table = toml_edit::InlineTable::new();
            for (k, v) in obj {
                if let Item::Value(val) = json_to_toml_value(v) {
                    table.insert(k, val);
                }
            }
            Item::Value(Value::InlineTable(table))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::layout::LayoutCommand;
    use crate::actor::reactor::Command as ReactorCommand;
    use crate::actor::wm_controller::{ExecCmd, WmCmd};
    use crate::model::{Direction, Orientation};
    use crate::ui::preferences_json::{PreferencesJson, command_json};

    /// The JSON that `ConfigBridge` in `SugargliderUI` sends to
    /// `sugarglider_update_config` and `sugarglider_save_config_to_file`.
    /// `PreferencesConfigTests.swift` checks that Swift still encodes it this
    /// way.
    const PREFERENCES_FROM_SWIFT: &str =
        include_str!("../tests/fixtures/preferences-from-swift.json");

    #[test]
    fn default_config_is_valid() {
        Config::default();
    }

    #[test]
    fn the_parked_window_journal_lives_next_to_the_layout() {
        assert_eq!(
            restore_file().with_file_name("parked.json"),
            parked_journal_file()
        );
        assert_eq!(data_dir().join("parked.json"), parked_journal_file());
    }

    #[test]
    fn contexts_live_next_to_the_layout() {
        assert_eq!(data_dir().join("contexts.json"), contexts_file());
    }

    #[test]
    fn toggle_global_enabled_is_default_key() {
        let config = Config::default();
        assert!(
            config.keys.iter().any(|(hk, cmd)| {
                hk.to_string() == "Alt + KeyZ"
                    && matches!(cmd, WmCommand::Wm(WmCmd::ToggleGlobalEnabled))
            }),
            "Alt+Z should be bound to toggle_global_enabled by default"
        );
    }

    #[test]
    fn toggle_space_activated_is_ctrl_alt_z() {
        let config = Config::default();
        assert!(
            config.keys.iter().any(|(hk, cmd)| {
                hk.to_string() == "Ctrl + Alt + KeyZ"
                    && matches!(cmd, WmCommand::Wm(WmCmd::ToggleSpaceActivated))
            }),
            "Ctrl+Alt+Z should be bound to toggle_space_activated by default"
        );
    }

    #[test]
    fn default_settings_match_unspecified_setting_values() {
        assert_eq!(Config::default().settings, Config::parse("").unwrap().settings);
    }

    #[test]
    fn size_share_bindings_parse() {
        let config = Config::default();
        let shares = config
            .keys
            .iter()
            .filter_map(|(hk, cmd)| match cmd {
                WmCommand::ReactorCommand(ReactorCommand::Layout(LayoutCommand::SetSizeShare(
                    share,
                ))) => Some((hk.to_string(), *share)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shares.len(),
            3,
            "expected three default size share bindings: {shares:?}"
        );
        assert!(
            shares
                .iter()
                .any(|(hk, share)| hk == "Ctrl + Shift + Digit2" && share.fraction() == Some(0.5)),
            "Ctrl+Shift+2 should take half the screen: {shares:?}"
        );
        assert!(
            shares
                .iter()
                .any(|(hk, share)| hk == "Ctrl + Shift + Digit3"
                    && share.fraction() == Some(1.0 / 3.0)),
            "Ctrl+Shift+3 should take a third of the screen: {shares:?}"
        );
        assert!(
            shares
                .iter()
                .any(|(hk, share)| hk == "Ctrl + Shift + Digit4" && share.fraction() == Some(0.25)),
            "Ctrl+Shift+4 should take a quarter of the screen: {shares:?}"
        );
    }

    #[test]
    fn size_share_overflow_defaults_to_reject() {
        assert_eq!(
            Config::default().settings.size_share.overflow,
            SizeShareOverflow::Reject
        );
        assert_eq!(
            Config::parse("settings.size_share.overflow = \"squeeze\"")
                .unwrap()
                .settings
                .size_share
                .overflow,
            SizeShareOverflow::Squeeze
        );
    }

    #[test]
    fn scroll_gate_is_disabled_by_default() {
        assert!(!Config::default().settings.experimental.scroll.enable);
    }

    /// In TOML, a bare integer names a context by number, a string by name,
    /// and `{ id = 7 }` by id.
    #[test]
    fn context_commands_parse() {
        use crate::actor::reactor::{ContextCommand, ContextRef, RecordRef};

        let config = Config::parse(
            r#"
            [keys]
            "Ctrl + Alt + Digit1" = { switch_context = 1 }
            "Ctrl + Alt + KeyC" = { switch_context = "Comms" }
            "Ctrl + Alt + KeyI" = { switch_context = { id = 7 } }
            "Ctrl + Alt + Digit0" = "show_everything"
            "Ctrl + Alt + Tab" = "previous_context"
            "Ctrl + Alt + KeyA" = { add_window_to_context = 2 }
            "Ctrl + Alt + KeyM" = { move_window_to_context = { id = 7 } }
            "Ctrl + Alt + KeyN" = { move_window_to_context = "Comms" }
            "Ctrl + Alt + KeyR" = "remove_window_from_context"
            "Ctrl + Alt + KeyP" = "toggle_window_pinned"
            "Ctrl + Alt + KeyD" = { delete_context = 3 }
            "Ctrl + Alt + KeyU" = { set_context_number = { context = "Comms", number = 2 } }
            "Ctrl + Alt + KeyF" = { remove_record = { context = { id = 7 }, record = { record = 0, app = "Mail", title = "Inbox" } } }
            "#,
        )
        .unwrap();
        let command = |key: &str| {
            config
                .keys
                .iter()
                .find(|(hotkey, _)| hotkey.to_string() == key)
                .map(|(_, cmd)| match cmd {
                    WmCommand::ReactorCommand(ReactorCommand::Context(cmd)) => cmd.clone(),
                    other => panic!("{other:?}"),
                })
                .unwrap()
        };
        let id = serde_json::from_value(serde_json::json!(7)).unwrap();
        assert_eq!(
            ContextCommand::SwitchContext(ContextRef::Number(1)),
            command("Ctrl + Alt + Digit1")
        );
        assert_eq!(
            ContextCommand::SwitchContext(ContextRef::Name("Comms".into())),
            command("Ctrl + Alt + KeyC")
        );
        assert_eq!(
            ContextCommand::SwitchContext(ContextRef::Id(id)),
            command("Ctrl + Alt + KeyI")
        );
        assert_eq!(ContextCommand::ShowEverything, command("Ctrl + Alt + Digit0"));
        assert_eq!(ContextCommand::PreviousContext, command("Ctrl + Alt + Tab"));
        assert_eq!(
            ContextCommand::AddWindowToContext(ContextRef::Number(2)),
            command("Ctrl + Alt + KeyA")
        );
        assert_eq!(
            ContextCommand::MoveWindowToContext(ContextRef::Id(id)),
            command("Ctrl + Alt + KeyM")
        );
        assert_eq!(
            ContextCommand::MoveWindowToContext(ContextRef::Name("Comms".into())),
            command("Ctrl + Alt + KeyN")
        );
        assert_eq!(
            ContextCommand::RemoveWindowFromContext,
            command("Ctrl + Alt + KeyR")
        );
        assert_eq!(ContextCommand::ToggleWindowPinned, command("Ctrl + Alt + KeyP"));
        assert_eq!(
            ContextCommand::DeleteContext(ContextRef::Number(3)),
            command("Ctrl + Alt + KeyD")
        );
        assert_eq!(
            ContextCommand::SetContextNumber {
                context: ContextRef::Name("Comms".into()),
                number: 2,
            },
            command("Ctrl + Alt + KeyU")
        );
        assert_eq!(
            ContextCommand::RemoveRecord {
                context: ContextRef::Id(id),
                record: RecordRef {
                    record: 0,
                    app: "Mail".into(),
                    title: "Inbox".into(),
                },
            },
            command("Ctrl + Alt + KeyF")
        );
    }

    /// Key bindings. The default config ships the context bindings of the
    /// spec commented out, so none is bound, and every shipped line parses
    /// once uncommented.
    #[test]
    fn the_default_config_ships_the_context_bindings_commented_out() {
        use crate::actor::reactor::{ContextCommand, ContextRef};

        let default_config = include_str!("../sugarglider.default.toml");
        let shipped = [
            r#"# "Ctrl + Alt + Space" = "open_context_switcher""#,
            r#"# "Ctrl + Alt + 0" = "show_everything""#,
            r#"# "Ctrl + Alt + 1" = { switch_context = 1 }"#,
            r#"# "Ctrl + Alt + 2" = { switch_context = 2 }"#,
            "# ... through 9",
            r#"# "Ctrl + Alt + Tab" = "previous_context""#,
        ]
        .join("\n");
        assert!(default_config.contains(&shipped));
        let context_bindings = |config: &Config| -> Vec<(String, ContextCommand)> {
            let mut bindings: Vec<_> = config
                .keys
                .iter()
                .filter_map(|(hotkey, cmd)| match cmd {
                    WmCommand::ReactorCommand(ReactorCommand::Context(cmd)) => {
                        Some((hotkey.to_string(), cmd.clone()))
                    }
                    _ => None,
                })
                .collect();
            bindings.sort_by(|a, b| a.0.cmp(&b.0));
            bindings
        };
        assert!(context_bindings(&Config::default()).is_empty());

        let uncommented: Vec<&str> = shipped
            .lines()
            .filter(|line| line.starts_with("# \""))
            .map(|line| &line[2..])
            .collect();
        let config = Config::parse(&format!("[keys]\n{}", uncommented.join("\n"))).unwrap();
        assert_eq!(
            vec![
                ("Ctrl + Alt + Digit0".to_string(), ContextCommand::ShowEverything),
                (
                    "Ctrl + Alt + Digit1".to_string(),
                    ContextCommand::SwitchContext(ContextRef::Number(1))
                ),
                (
                    "Ctrl + Alt + Digit2".to_string(),
                    ContextCommand::SwitchContext(ContextRef::Number(2))
                ),
                (
                    "Ctrl + Alt + Space".to_string(),
                    ContextCommand::OpenContextSwitcher
                ),
                ("Ctrl + Alt + Tab".to_string(), ContextCommand::PreviousContext),
            ],
            context_bindings(&config)
        );
    }

    /// R28. Contexts are off by default and choose global scope; the scope key
    /// is read and rejects values that aren't a scope.
    #[test]
    fn contexts_are_off_by_default_and_turn_on_with_their_flag() {
        assert!(!Config::default().settings.experimental.contexts.enable);
        assert_eq!(
            Scope::Global,
            Config::default().settings.experimental.contexts.scope
        );
        let config = Config::parse("settings.experimental.contexts.enable = true").unwrap();
        assert!(config.settings.experimental.contexts.enable);
        let scoped =
            Config::parse("settings.experimental.contexts.scope = \"per_screen\"").unwrap();
        assert_eq!(Scope::PerScreen, scoped.settings.experimental.contexts.scope);
        assert!(Config::parse("settings.experimental.contexts.scope = \"sometimes\"").is_err());
    }

    #[test]
    fn window_rules_are_empty_by_default() {
        assert!(Config::default().window_rules.is_empty());
    }

    #[test]
    fn window_rules_parse() {
        let config = Config::parse(
            r#"
            window_rules = [
              { if = { app_id = "com.example.X", title_regex = "Dialog" }, float = true },
              { if = { title_substring = "Preferences", ax_subrole = "AXDialog" }, float = true },
            ]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.window_rules,
            vec![
                WindowRule {
                    conditions: WindowRuleConditions {
                        app_id: Some("com.example.X".into()),
                        title_regex: Some("Dialog".parse().unwrap()),
                        ..Default::default()
                    },
                    float: true,
                },
                WindowRule {
                    conditions: WindowRuleConditions {
                        title_substring: Some("Preferences".into()),
                        ax_subrole: Some("AXDialog".into()),
                        ..Default::default()
                    },
                    float: true,
                },
            ]
        );
    }

    #[test]
    fn window_rules_parse_with_dotted_if_keys() {
        // The `if.app_id` dotted-key form from the aerospace-style API.
        let config = Config::parse(
            r#"
            [[window_rules]]
            if.app_id = "com.example.X"
            float = true
            "#,
        )
        .unwrap();
        assert_eq!(
            config.window_rules,
            vec![WindowRule {
                conditions: WindowRuleConditions {
                    app_id: Some("com.example.X".into()),
                    ..Default::default()
                },
                float: true,
            }]
        );
    }

    #[test]
    fn window_rule_invalid_regex_is_rejected() {
        let err = Config::parse(
            r#"
            window_rules = [{ if = { title_regex = "(unterminated" }, float = true }]
            "#,
        )
        .unwrap_err();
        assert!(
            err.message.contains("regex"),
            "unexpected error: {}",
            err.message
        );
    }

    #[test]
    fn default_keys_exclude_scroll_experimental_commands() {
        let config = Config::default();
        assert!(!config.keys.iter().any(|(_, cmd)| {
            matches!(
                cmd,
                WmCommand::ReactorCommand(ReactorCommand::Layout(
                    LayoutCommand::ChangeLayoutKind
                        | LayoutCommand::ToggleColumnTabbed
                        | LayoutCommand::CycleColumnWidth
                ))
            )
        }));
    }

    #[test]
    fn default_keys_false_excludes_default_bindings() {
        let config = Config::parse(
            r#"
            [settings]
            default_keys = false

            [keys]
            "Alt + Q" = "debug"
            "#,
        )
        .unwrap();

        // Should only have our custom key, not the defaults
        assert_eq!(config.keys.len(), 1);
        let (hotkey, _cmd) = &config.keys[0];
        assert_eq!(hotkey.to_string(), "Alt + KeyQ");
    }

    #[test]
    fn default_keys_true_includes_default_bindings() {
        let config = Config::parse(
            r#"
            [settings]
            default_keys = true

            [keys]
            "Alt + Q" = "debug"
            "#,
        )
        .unwrap();

        // Should have default keys plus our custom key
        let default_key_count = Config::default().keys.len();
        assert_eq!(config.keys.len(), default_key_count + 1);

        // Our custom key should be present
        assert!(config.keys.iter().any(|(hk, _)| hk.to_string() == "Alt + KeyQ"));
    }

    #[test]
    fn missing_keys_section_includes_default_bindings() {
        let config = Config::parse(
            r#"
            [settings]
            animate = false
            "#,
        )
        .unwrap();

        assert_eq!(config.keys.len(), Config::default().keys.len());
    }

    #[test]
    fn disable_removes_key_binding() {
        let config = Config::parse(
            r#"
            [settings]
            default_keys = false

            [keys]
            "Alt + Q" = "debug"
            "Alt + W" = "disable"
            "#,
        )
        .unwrap();

        // "disable" key should not appear in final config
        assert_eq!(config.keys.len(), 1);
        assert!(config.keys.iter().any(|(hk, _)| hk.to_string() == "Alt + KeyQ"));
        assert!(!config.keys.iter().any(|(hk, _)| hk.to_string() == "Alt + KeyW"));
    }

    #[test]
    fn disable_can_override_default_key() {
        // First verify Alt+T exists in defaults
        let default_config = Config::default();
        assert!(
            default_config.keys.iter().any(|(hk, _)| hk.to_string() == "Alt + KeyT"),
            "Alt+T should be a default key binding"
        );

        let config = Config::parse(
            r#"
            [settings]
            default_keys = true

            [keys]
            "Alt + T" = "disable"
            "#,
        )
        .unwrap();

        // Alt+T should be removed even though it's in defaults
        assert!(!config.keys.iter().any(|(hk, _)| hk.to_string() == "Alt + KeyT"));
        // But other default keys should still be present
        assert!(config.keys.iter().any(|(hk, _)| hk.to_string() == "Alt + Slash"));
    }

    /// A key replaces or disables the default binding of the same hotkey,
    /// however the file spells the hotkey.
    #[test]
    fn keys_replace_default_bindings_spelled_differently() {
        let config = Config::parse(
            r#"
            [settings]
            default_keys = true

            [keys]
            "Alt + KeyT" = "disable"
            "Ctrl + Alt + KeyH" = "debug"
            "#,
        )
        .unwrap();

        let bound_to = |key: &str| -> Vec<serde_json::Value> {
            let hotkey = Hotkey::from_str(key).unwrap();
            let bindings = config.keys.iter().filter(|(hk, _)| *hk == hotkey);
            bindings.map(|(_, cmd)| command_json(cmd)).collect()
        };
        assert_eq!(Vec::<serde_json::Value>::new(), bound_to("Alt + T"));
        assert_eq!(vec![serde_json::json!("debug")], bound_to("Alt + Ctrl + H"));
        assert_eq!(Config::default().keys.len() - 1, config.keys.len());
    }

    /// Two spellings of one hotkey in one file bind the command of the last
    /// entry, in the order the file writes them. With `default_keys`, the
    /// same hotkey of the defaults is replaced too.
    #[test]
    fn the_last_spelling_of_a_hotkey_binds_its_command() {
        for default_keys in [false, true] {
            let config = Config::parse(&format!(
                r#"
                [settings]
                default_keys = {default_keys}

                [keys]
                "Alt + T" = "debug"
                "Alt + KeyT" = {{ group = "vertical" }}
                "#,
            ))
            .unwrap();

            let alt_t = Hotkey::from_str("Alt + T").unwrap();
            let bindings: Vec<&WmCommand> = config
                .keys
                .iter()
                .filter(|(hotkey, _)| *hotkey == alt_t)
                .map(|(_, cmd)| cmd)
                .collect();
            assert_eq!(1, bindings.len(), "default_keys = {default_keys}");
            assert_eq!(
                command_json(&WmCommand::ReactorCommand(ReactorCommand::Layout(
                    LayoutCommand::Group(Orientation::Vertical)
                ))),
                command_json(bindings[0]),
                "default_keys = {default_keys}"
            );
            let default_count = Config::default().keys.len();
            assert_eq!(
                if default_keys { default_count } else { 1 },
                config.keys.len(),
                "default_keys = {default_keys}"
            );
        }
    }

    #[test]
    fn exec_cmd_options_parse() {
        let config = Config::parse(
            r#"
            [settings]
            default_keys = false

            [keys]
            "Alt + Q" = { exec = ["bash", "-c", "echo hi"] }
            "Alt + W" = { exec = { cmd = ["bash", "-c", "echo hi"], unsafe_privileged = true } }
            "#,
        )
        .unwrap();

        let cmd_q = &config
            .keys
            .iter()
            .find(|(hk, _)| hk.to_string() == "Alt + KeyQ")
            .expect("Alt + KeyQ should be present")
            .1;
        let WmCommand::Wm(WmCmd::Exec(exec_cmd)) = cmd_q else {
            panic!("Expected exec command; got {cmd_q:?}");
        };
        assert_eq!(
            exec_cmd.clone().normalize(),
            crate::actor::wm_controller::NormalizedExecCmd {
                cmd_args: vec!["bash".to_owned(), "-c".to_owned(), "echo hi".to_owned()],
                unsafe_privileged: false,
            }
        );

        let cmd_w = &config
            .keys
            .iter()
            .find(|(hk, _)| hk.to_string() == "Alt + KeyW")
            .expect("Alt + KeyW should be present")
            .1;
        let WmCommand::Wm(WmCmd::Exec(exec_cmd)) = cmd_w else {
            panic!("Expected exec command; got {cmd_w:?}");
        };
        assert_eq!(
            exec_cmd.clone().normalize(),
            crate::actor::wm_controller::NormalizedExecCmd {
                cmd_args: vec!["bash".to_owned(), "-c".to_owned(), "echo hi".to_owned()],
                unsafe_privileged: true,
            }
        );
    }

    #[test]
    fn aspect_ratio_from_str_valid() {
        let ar = AspectRatio::from_str("16:9").unwrap();
        assert_eq!(ar.width, 16.0);
        assert_eq!(ar.height, 9.0);
    }

    #[test]
    fn aspect_ratio_from_str_with_spaces() {
        let ar = AspectRatio::from_str(" 4 : 3 ").unwrap();
        assert_eq!(ar.width, 4.0);
        assert_eq!(ar.height, 3.0);
    }

    #[test]
    fn aspect_ratio_from_str_invalid() {
        assert!(AspectRatio::from_str("16x9").is_err());
        assert!(AspectRatio::from_str("0:9").is_err());
        assert!(AspectRatio::from_str("16:-1").is_err());
        assert!(AspectRatio::from_str("abc:def").is_err());
    }

    #[test]
    fn arrow_keys_parse_correctly() {
        let config = Config::parse(
            r#"
            [settings]
            default_keys = false

            [keys]
            "Alt + ArrowLeft" = { move_focus = "left" }
            "Alt + ArrowDown" = { move_focus = "down" }
            "Alt + ArrowUp" = { move_focus = "up" }
            "Alt + ArrowRight" = { move_focus = "right" }
            "#,
        )
        .unwrap();

        // Should have all 4 arrow key bindings
        assert_eq!(config.keys.len(), 4);

        // Verify all arrow keys are present
        assert!(config.keys.iter().any(|(hk, _)| hk.to_string() == "Alt + ArrowLeft"));
        assert!(config.keys.iter().any(|(hk, _)| hk.to_string() == "Alt + ArrowDown"));
        assert!(config.keys.iter().any(|(hk, _)| hk.to_string() == "Alt + ArrowUp"));
        assert!(config.keys.iter().any(|(hk, _)| hk.to_string() == "Alt + ArrowRight"));
    }

    #[test]
    fn clean_up_space_is_default_key() {
        let config = Config::default();
        assert!(
            config.keys.iter().any(|(hk, cmd)| {
                hk.to_string() == "Alt + Shift + KeyC"
                    && matches!(
                        cmd,
                        WmCommand::ReactorCommand(ReactorCommand::Layout(
                            LayoutCommand::CleanUpSpace
                        ))
                    )
            }),
            "Alt+Shift+C should be bound to clean_up_space by default"
        );
    }

    #[test]
    fn parse_hotkey_string_requires_modifiers() {
        // parse_hotkey_string (in preferences_json.rs) requires at least one modifier
        // This test verifies that behavior through the public API
        use crate::ui::preferences_json::PreferencesJson;

        // Create a config with default keys
        let config = Config::default();
        let prefs_json = PreferencesJson::from_config(&config);

        // Verify all default hotkeys have modifiers (contain a modifier symbol)
        for hk in &prefs_json.hotkeys {
            assert!(
                hk.key.contains('⌥')
                    || hk.key.contains('⌃')
                    || hk.key.contains('⇧')
                    || hk.key.contains('⌘'),
                "Hotkey '{}' for command {} should have at least one modifier",
                hk.key,
                hk.command
            );
        }
    }

    /// The Swift `HotkeyBinding` has no sort order, so the key bindings in
    /// the Preferences window's JSON have none. Each binding carries its
    /// command, which the file then holds.
    #[test]
    fn preferences_from_the_swift_ui_save_with_their_key_bindings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glide.toml");
        std::fs::write(&path, "[keys]\n\"Alt + T\" = { exec = \"open -a Terminal\" }\n").unwrap();

        let prefs: PreferencesJson = serde_json::from_str(PREFERENCES_FROM_SWIFT).unwrap();
        write_preferences_to_path(&prefs, &path).unwrap();

        let config = Config::load(Some(&path)).unwrap();
        assert!(!config.settings.animate);
        assert!(config.settings.focus_follows_mouse);
        assert_eq!(8.0, config.settings.outer_gap);
        assert_eq!(4.0, config.settings.inner_gap);
        let app_names: Vec<_> = config
            .window_rules
            .iter()
            .map(|rule| rule.conditions.app_name.as_deref())
            .collect();
        assert_eq!(vec![Some("Finder"), Some("Calculator")], app_names);
        let bound_to = |key: &str| {
            let hotkey = Hotkey::from_str(key).unwrap();
            let binding = config.keys.iter().find(|(hk, _)| *hk == hotkey);
            binding.map(|(_, cmd)| cmd).unwrap_or_else(|| panic!("{key} is not bound"))
        };
        assert!(matches!(
            bound_to("Alt + KeyZ"),
            WmCommand::Wm(WmCmd::ToggleGlobalEnabled)
        ));
        assert!(matches!(
            bound_to("Ctrl + Alt + Shift + KeyH"),
            WmCommand::ReactorCommand(ReactorCommand::Layout(LayoutCommand::MoveFocus(
                Direction::Left
            )))
        ));
        assert!(matches!(
            bound_to("Alt + KeyT"),
            WmCommand::Wm(WmCmd::Exec(ExecCmd::String(cmd))) if cmd == "open -a Terminal"
        ));
    }

    /// A save that changes no window rule leaves the file's
    /// `[[window_rules]]` tables alone, comments included, and a rule's
    /// conditions stay in the running config too.
    #[test]
    fn preferences_keep_window_rule_conditions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glide.toml");
        std::fs::write(
            &path,
            "# Float the picture-in-picture window.\n\
             [[window_rules]]\n\
             if = { title_regex = \"Picture-in-Picture\" }\n\
             float = true\n",
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(1, config.window_rules.len());

        let mut prefs = preferences_for(&config);
        prefs.animate = !prefs.animate;
        let running = prefs.apply_to_config(&config);
        assert_eq!(config.window_rules, running.window_rules);

        write_preferences_to_path(&prefs, &path).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("# Float the picture-in-picture window."),
            "{written}"
        );
        assert!(
            written.contains("title_regex = \"Picture-in-Picture\""),
            "{written}"
        );
        let saved = Config::load(Some(&path)).unwrap();
        assert_eq!(config.window_rules, saved.window_rules);
    }

    /// A window rule whose condition is an empty string keeps it: a save
    /// that changes no rule leaves the file alone, and the running config
    /// keeps the same condition. `app_id = ""` matches only an empty bundle
    /// id, so dropping the condition would widen the rule to every window.
    #[test]
    fn preferences_keep_empty_window_rule_conditions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glide.toml");
        std::fs::write(
            &path,
            "[[window_rules]]\n\
             if = { app_id = \"\" }\n\
             float = true\n",
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(Some(String::new()), config.window_rules[0].conditions.app_id);

        let mut prefs = preferences_for(&config);
        prefs.animate = !prefs.animate;
        assert_eq!(config.window_rules, prefs.apply_to_config(&config).window_rules);

        write_preferences_to_path(&prefs, &path).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("app_id = \"\""), "{written}");
        assert_eq!(
            config.window_rules,
            Config::load(Some(&path)).unwrap().window_rules
        );
    }

    /// When a window rule change does rewrite the rules, an empty condition
    /// is written as it stands, so the rules that stay keep their meaning.
    #[test]
    fn preferences_write_empty_window_rule_conditions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glide.toml");
        std::fs::write(
            &path,
            "[[window_rules]]\n\
             if = { app_id = \"\" }\n\
             float = true\n\n\
             [[window_rules]]\n\
             if = { app_name = \"Finder\" }\n\
             float = false\n",
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(2, config.window_rules.len());

        let mut prefs = preferences_for(&config);
        prefs.window_rules.remove(1); // the window deleted the Finder rule
        write_preferences_to_path(&prefs, &path).unwrap();

        let saved = Config::load(Some(&path)).unwrap();
        assert_eq!(vec![config.window_rules[0].clone()], saved.window_rules);
    }

    /// A window rule that the App Rules pane removes goes away, and the
    /// rules that stay keep their conditions in the file and the running
    /// config.
    #[test]
    fn preferences_write_window_rule_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glide.toml");
        std::fs::write(
            &path,
            "[[window_rules]]\n\
             if = { title_regex = \"Picture-in-Picture\" }\n\
             float = true\n\n\
             [[window_rules]]\n\
             if = { app_id = \"com.example.X\" }\n\
             float = false\n",
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(2, config.window_rules.len());

        let mut prefs = preferences_for(&config);
        prefs.window_rules.remove(0);
        write_preferences_to_path(&prefs, &path).unwrap();

        let saved = Config::load(Some(&path)).unwrap();
        assert_eq!(vec![config.window_rules[1].clone()], saved.window_rules);
        assert_eq!(prefs.apply_to_config(&config).window_rules, saved.window_rules);
    }

    /// A config file with an error stays as it is, and the error says what
    /// is wrong without terminal colors.
    #[test]
    fn preferences_never_overwrite_a_config_file_with_an_error() {
        let files = [
            // Not TOML.
            ("[settings]\nanimate = tru\n", "could not parse config"),
            // Not a setting.
            ("[settings]\nanimates = false\n", "could not parse config"),
            // Not a key.
            ("[keys]\n\"Alt + Nope\" = \"debug\"\n", "Could not parse hotkey"),
            // TOML 1.1, which toml_edit can't edit.
            (
                "[settings]\nexperimental = { scroll = { enable = true, } }\n",
                "TOML parse error",
            ),
        ];
        let prefs: PreferencesJson = serde_json::from_str(PREFERENCES_FROM_SWIFT).unwrap();
        for (file, message) in files {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("glide.toml");
            std::fs::write(&path, file).unwrap();

            let error = write_preferences_to_path(&prefs, &path).unwrap_err().to_string();

            assert_eq!(file, std::fs::read_to_string(&path).unwrap());
            assert_eq!(1, std::fs::read_dir(dir.path()).unwrap().count(), "{file}");
            assert!(error.contains(message), "{error}");
            assert!(error.contains("has an error, so it was not changed"), "{error}");
            assert!(!error.contains('\u{1b}'), "{error}");
        }
    }

    /// The Preferences window's JSON for `config`, as Swift sends it back.
    fn preferences_for(config: &Config) -> PreferencesJson {
        let json = serde_json::to_string(&PreferencesJson::from_config(config)).unwrap();
        let mut prefs: PreferencesJson = serde_json::from_str(&json).unwrap();
        for hotkey in &mut prefs.hotkeys {
            hotkey.sort_order = 0;
        }
        prefs
    }

    /// Changes the key of the binding on `from` to `to`.
    fn rebind(prefs: &mut PreferencesJson, from: &str, to: &str) {
        let binding = prefs.hotkeys.iter_mut().find(|hk| hk.key == from);
        binding.unwrap_or_else(|| panic!("{from} is not bound")).key = to.to_string();
    }

    /// Two `exec` bindings, and two `resize` bindings that differ only in
    /// their percent, stay apart when Preferences changes a key, in the
    /// running config and in the saved file.
    #[test]
    fn preferences_keep_bindings_of_the_same_command_apart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glide.toml");
        std::fs::write(
            &path,
            r#"
            [keys]
            "Alt + Q" = { exec = "open -a Terminal" }
            "Alt + W" = { exec = "open -a Safari" }
            "Alt + Ctrl + H" = { resize = { direction = "left", percent = 5 } }
            "Alt + Ctrl + Shift + H" = { resize = { direction = "left", percent = 10 } }
            "#,
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();
        let expected = Config::parse(
            r#"
            [keys]
            "Alt + E" = { exec = "open -a Terminal" }
            "Alt + W" = { exec = "open -a Safari" }
            "Alt + Ctrl + H" = { resize = { direction = "left", percent = 5 } }
            "Alt + Ctrl + Shift + Y" = { resize = { direction = "left", percent = 10 } }
            "#,
        )
        .unwrap();

        let mut prefs = preferences_for(&config);
        rebind(&mut prefs, "⌥Q", "⌥E");
        rebind(&mut prefs, "⌃⌥⇧H", "⌃⌥⇧Y");

        let running = prefs.apply_to_config(&config);
        assert_eq!(sorted_bindings(&expected.keys), sorted_bindings(&running.keys));
        write_preferences_to_path(&prefs, &path).unwrap();
        let saved = Config::load(Some(&path)).unwrap();
        assert_eq!(sorted_bindings(&expected.keys), sorted_bindings(&saved.keys));

        // The window keeps its bindings for the next change.
        rebind(&mut prefs, "⌥W", "⌥R");
        let expected = Config::parse(
            r#"
            [keys]
            "Alt + E" = { exec = "open -a Terminal" }
            "Alt + R" = { exec = "open -a Safari" }
            "Alt + Ctrl + H" = { resize = { direction = "left", percent = 5 } }
            "Alt + Ctrl + Shift + Y" = { resize = { direction = "left", percent = 10 } }
            "#,
        )
        .unwrap();

        let running = prefs.apply_to_config(&running);
        assert_eq!(sorted_bindings(&expected.keys), sorted_bindings(&running.keys));
        write_preferences_to_path(&prefs, &path).unwrap();
        let saved = Config::load(Some(&path)).unwrap();
        assert_eq!(sorted_bindings(&expected.keys), sorted_bindings(&saved.keys));
    }

    /// With `default_keys = true`, the "disable" entries and their comments
    /// stay when Preferences saves, and a default binding moved to another
    /// key doesn't come back on its old key.
    #[test]
    fn preferences_keep_disabled_keys_with_default_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glide.toml");
        let disabled = [
            "# No tabs.",
            "\"Alt + T\" = \"disable\" # Alt + S still stacks.",
            "\"Alt + W\" = \"disable\"",
        ];
        let file = format!(
            "[settings]\ndefault_keys = true\n\n[keys]\n{}\n",
            disabled.join("\n")
        );
        std::fs::write(&path, file).unwrap();
        let config = Config::load(Some(&path)).unwrap();
        let alt_t = Hotkey::from_str("Alt + T").unwrap();
        let alt_s = Hotkey::from_str("Alt + S").unwrap();
        assert!(!config.keys.iter().any(|(hotkey, _)| *hotkey == alt_t));

        let mut prefs = preferences_for(&config);
        prefs.animate = !prefs.animate;
        write_preferences_to_path(&prefs, &path).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let saved = Config::load(Some(&path)).unwrap();
        assert_eq!(
            sorted_bindings(&config.keys),
            sorted_bindings(&saved.keys),
            "{written}"
        );
        for line in disabled {
            assert!(written.contains(line), "{written}");
        }

        rebind(&mut prefs, "⌥S", "⌥G");
        write_preferences_to_path(&prefs, &path).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let saved = Config::load(Some(&path)).unwrap();
        let running = prefs.apply_to_config(&config);
        assert_eq!(
            sorted_bindings(&running.keys),
            sorted_bindings(&saved.keys),
            "{written}"
        );
        assert!(!saved.keys.iter().any(|(hotkey, _)| [alt_s, alt_t].contains(hotkey)));
        for line in disabled {
            assert!(written.contains(line), "{written}");
        }
    }

    /// Every default binding comes back unchanged through the Preferences
    /// window's JSON, so a save that changes no binding finds none changed.
    #[test]
    fn default_bindings_survive_the_preferences_json() {
        let config = Config::default();
        let prefs = preferences_for(&config);
        assert_eq!(config.keys.len(), prefs.hotkeys.len());
        assert_eq!(sorted_bindings(&config.keys), sorted_bindings(&prefs.bindings()));
    }

    /// The `[keys]` table of a saved file, with each key parsed and each
    /// value as JSON.
    fn saved_keys(written: &str) -> Vec<(String, String)> {
        let table: toml::Table = toml::from_str(written).unwrap();
        let mut keys: Vec<_> = table["keys"]
            .as_table()
            .unwrap()
            .iter()
            .map(|(key, value)| {
                let hotkey = Hotkey::from_str(key).unwrap().to_string();
                (hotkey, serde_json::to_value(value).unwrap().to_string())
            })
            .collect();
        keys.sort();
        keys
    }

    /// Changing only a setting leaves the key bindings of the file alone,
    /// so a file without `[keys]` gets none.
    #[test]
    fn preferences_add_no_keys_when_no_binding_changed() {
        let files = [
            None,
            Some("[settings]\nanimate = true\n"),
            Some("[settings]\ndefault_keys = true\n"),
        ];
        for file in files {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("glide.toml");
            if let Some(file) = file {
                std::fs::write(&path, file).unwrap();
            }
            let config = Config::parse(file.unwrap_or_default()).unwrap();

            let mut prefs = preferences_for(&config);
            prefs.animate = false;
            write_preferences_to_path(&prefs, &path).unwrap();

            let written = std::fs::read_to_string(&path).unwrap();
            let table: toml::Table = toml::from_str(&written).unwrap();
            assert!(!table.contains_key("keys"), "{written}");
            let saved = Config::load(Some(&path)).unwrap();
            assert!(!saved.settings.animate);
            assert_eq!(sorted_bindings(&config.keys), sorted_bindings(&saved.keys));
        }
    }

    /// With `default_keys = true`, `[keys]` holds only what differs from the
    /// defaults: a moved default binding on its new key, and "disable" on
    /// its old key. Moving it back empties the table.
    #[test]
    fn preferences_save_only_changed_bindings_with_default_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glide.toml");
        std::fs::write(&path, "[settings]\ndefault_keys = true\n").unwrap();
        let config = Config::load(Some(&path)).unwrap();

        let mut prefs = preferences_for(&config);
        rebind(&mut prefs, "⌥S", "⌥G");
        write_preferences_to_path(&prefs, &path).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            vec![
                ("Alt + KeyG".to_string(), r#"{"group":"vertical"}"#.to_string()),
                ("Alt + KeyS".to_string(), r#""disable""#.to_string()),
            ],
            saved_keys(&written),
            "{written}"
        );
        let saved = Config::load(Some(&path)).unwrap();
        let running = prefs.apply_to_config(&config);
        assert_eq!(sorted_bindings(&running.keys), sorted_bindings(&saved.keys));

        rebind(&mut prefs, "⌥G", "⌥S");
        write_preferences_to_path(&prefs, &path).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(Vec::<(String, String)>::new(), saved_keys(&written), "{written}");
        let saved = Config::load(Some(&path)).unwrap();
        assert_eq!(sorted_bindings(&config.keys), sorted_bindings(&saved.keys));
    }

    /// With `default_keys = false`, `[keys]` holds every binding once one
    /// changes, and an entry that stays keeps its comment. A file without
    /// `[keys]` has the default bindings, which it then holds.
    #[test]
    fn preferences_save_every_binding_without_default_keys() {
        let files = [
            (
                "[settings]\ndefault_keys = false\n\n[keys]\n# Terminal\n\
                 \"Alt + Q\" = { exec = \"open -a Terminal\" }\n\"Alt + W\" = \"debug\"\n",
                ("⌥W", "⌥E"),
            ),
            ("[settings]\nanimate = true\n", ("⌥S", "⌥G")),
        ];
        for (file, (from, to)) in files {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("glide.toml");
            std::fs::write(&path, file).unwrap();
            let config = Config::load(Some(&path)).unwrap();

            let mut prefs = preferences_for(&config);
            rebind(&mut prefs, from, to);
            write_preferences_to_path(&prefs, &path).unwrap();

            let written = std::fs::read_to_string(&path).unwrap();
            let running = prefs.apply_to_config(&config);
            assert_eq!(sorted_bindings(&running.keys), saved_keys(&written), "{written}");
            let saved = Config::load(Some(&path)).unwrap();
            assert_eq!(sorted_bindings(&running.keys), sorted_bindings(&saved.keys));
            for comment in file.lines().filter(|line| line.starts_with('#')) {
                assert!(written.contains(comment), "{written}");
            }
        }
    }

    /// A `[keys]` entry whose command changes keeps its spellings and
    /// comments: the writer copies the entry's decor onto the new value.
    #[test]
    fn preferences_keep_the_comment_of_a_binding_whose_command_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glide.toml");
        std::fs::write(
            &path,
            "[settings]\ndefault_keys = false\n\n[keys]\n\
             # Terminal\n\
             \"Alt + Q\" = { exec = \"open -a Terminal\" } # was debug\n",
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();

        let mut prefs = preferences_for(&config);
        prefs
            .hotkeys
            .iter_mut()
            .find(|hotkey| hotkey.key == "⌥Q")
            .expect("Alt + Q is shown")
            .command = r#"{"exec":"open -a Safari"}"#.to_string();

        write_preferences_to_path(&prefs, &path).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# Terminal"), "{written}");
        assert!(written.contains("# was debug"), "{written}");
        let saved = Config::load(Some(&path)).unwrap();
        assert_eq!(
            command_json(&WmCommand::Wm(WmCmd::Exec(ExecCmd::String(
                "open -a Safari".to_string()
            )))),
            command_json(&saved.keys[0].1),
            "{written}"
        );
    }

    /// A context command survives the Preferences JSON, the file it writes,
    /// and a fresh load.
    #[test]
    fn context_bindings_survive_json_toml_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glide.toml");
        std::fs::write(
            &path,
            "[settings]\ndefault_keys = false\n\n[keys]\n\
             \"Ctrl + Alt + Digit7\" = { switch_context = 7 }\n\
             \"Ctrl + Alt + KeyC\" = { switch_context = \"Comms\" }\n",
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();

        let mut prefs = preferences_for(&config);
        rebind(&mut prefs, "⌃⌥7", "⌃⌥8");
        write_preferences_to_path(&prefs, &path).unwrap();

        let saved = Config::load(Some(&path)).unwrap();
        assert_eq!(
            sorted_bindings(&prefs.apply_to_config(&config).keys),
            sorted_bindings(&saved.keys)
        );
        let commands: Vec<String> =
            saved.keys.iter().map(|(_, cmd)| command_json(cmd).to_string()).collect();
        assert!(commands.contains(&r#"{"switch_context":7}"#.to_string()));
        assert!(commands.contains(&r#"{"switch_context":"Comms"}"#.to_string()));
    }

    fn leaf_keys(prefix: &str, table: &toml::Table, keys: &mut Vec<String>) {
        for (key, value) in table {
            let path = format!("{prefix}.{key}");
            match value {
                toml::Value::Table(table) => leaf_keys(&path, table, keys),
                _ => keys.push(path),
            }
        }
    }

    /// The Preferences window writes only its fixed list of settings, the
    /// contexts switch among them, and the config file reads the switch back.
    #[test]
    fn the_contexts_switch_saves_with_only_the_fixed_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glide.toml");
        let mut prefs: PreferencesJson = serde_json::from_str(PREFERENCES_FROM_SWIFT).unwrap();
        assert!(prefs.contexts_enable);

        write_preferences_to_path(&prefs, &path).unwrap();

        let written: toml::Table =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut top_level: Vec<&str> = written.keys().map(String::as_str).collect();
        top_level.sort();
        assert_eq!(vec!["keys", "settings", "window_rules"], top_level);
        let mut settings = Vec::new();
        leaf_keys(
            "settings",
            written["settings"].as_table().unwrap(),
            &mut settings,
        );
        settings.sort();
        assert_eq!(
            vec![
                "settings.animate",
                "settings.default_layout_kind",
                "settings.drag_drop.enable",
                "settings.drag_drop.live_preview",
                "settings.experimental.contexts.enable",
                "settings.experimental.contexts.scope",
                "settings.focus_follows_mouse",
                "settings.inner_gap",
                "settings.mouse_follows_focus",
                "settings.outer_gap",
                "settings.status_icon.enable",
            ],
            settings
        );
        assert!(Config::load(Some(&path)).unwrap().settings.experimental.contexts.enable);

        prefs.contexts_enable = false;
        write_preferences_to_path(&prefs, &path).unwrap();
        assert!(!Config::load(Some(&path)).unwrap().settings.experimental.contexts.enable);
    }

    #[test]
    fn the_scope_picker_saves_the_scope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glide.toml");
        std::fs::write(
            &path,
            "[settings.experimental]\n# Keep this comment.\ncontexts.enable = true\n",
        )
        .unwrap();
        let mut prefs: PreferencesJson = serde_json::from_str(PREFERENCES_FROM_SWIFT).unwrap();
        prefs.contexts_scope = Scope::PerScreen;

        write_preferences_to_path(&prefs, &path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# Keep this comment."));
        assert_eq!(
            Scope::PerScreen,
            Config::load(Some(&path)).unwrap().settings.experimental.contexts.scope
        );

        prefs.contexts_scope = Scope::Global;
        write_preferences_to_path(&prefs, &path).unwrap();
        assert_eq!(
            Scope::Global,
            Config::load(Some(&path)).unwrap().settings.experimental.contexts.scope
        );
    }

    /// The drag-and-drop switches save in each form a file can give the
    /// `drag_drop` table, next to its other settings and comments.
    #[test]
    fn the_drag_and_drop_switches_save() {
        let files = [
            "",
            "[settings.drag_drop]\n# Pixels.\ndrag_threshold = 10.0\n",
            "[settings]\ndrag_drop.drag_threshold = 10.0\n",
            "[settings]\ndrag_drop = { drag_threshold = 10.0 }\n",
            "settings.drag_drop.drag_threshold = 10.0\n",
        ];
        let mut prefs: PreferencesJson = serde_json::from_str(PREFERENCES_FROM_SWIFT).unwrap();
        for file in files {
            for (enable, live_preview) in [(false, true), (true, false)] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("glide.toml");
                std::fs::write(&path, file).unwrap();
                prefs.drag_drop_enable = enable;
                prefs.drag_drop_live_preview = live_preview;

                write_preferences_to_path(&prefs, &path).unwrap();

                let written = std::fs::read_to_string(&path).unwrap();
                let config = Config::load(Some(&path)).unwrap_or_else(|e| panic!("{e}\n{written}"));
                let drag_drop = config.settings.drag_drop;
                assert_eq!(enable, drag_drop.enable, "{written}");
                assert_eq!(live_preview, drag_drop.live_preview, "{written}");
                if !file.is_empty() {
                    assert_eq!(10.0, drag_drop.drag_threshold, "{written}");
                }
                for comment in file.lines().filter(|line| line.starts_with('#')) {
                    assert!(written.contains(comment), "{written}");
                }
            }
        }
    }

    /// Saving the contexts switch keeps the file's comments and other
    /// experimental settings in each form a file can give the `experimental`
    /// table, and never adds a second `contexts` table.
    #[test]
    fn the_contexts_switch_saves_into_the_existing_experimental_table() {
        let files = [
            // The form that sugarglider.default.toml uses.
            "[settings.experimental]\n\n# Scroll layout settings.\nscroll.enable = true\n\n\
             # Named window sets.\ncontexts.enable = false\n",
            "[settings.experimental.scroll]\nenable = true\n",
            "[settings]\nexperimental.scroll.enable = true\n",
            "[settings]\nexperimental = { scroll = { enable = true } }\n",
        ];
        let prefs: PreferencesJson = serde_json::from_str(PREFERENCES_FROM_SWIFT).unwrap();
        for file in files {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("glide.toml");
            std::fs::write(&path, file).unwrap();

            write_preferences_to_path(&prefs, &path).unwrap();

            let written = std::fs::read_to_string(&path).unwrap();
            let config = Config::load(Some(&path)).unwrap_or_else(|e| panic!("{e}\n{written}"));
            assert!(config.settings.experimental.contexts.enable, "{written}");
            assert!(config.settings.experimental.scroll.enable, "{written}");
            for comment in file.lines().filter(|line| line.starts_with('#')) {
                assert!(written.contains(comment), "{written}");
            }
        }
    }
}

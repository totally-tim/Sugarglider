// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Defines the [`LayoutManager`] actor.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use redact::Secret;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, warn};

use crate::actor::app::{WindowId, pid_t};
use crate::collections::{BTreeExt, BTreeSet, HashMap, HashSet};
use crate::config::{
    Config, NewWindowPlacement, ScrollConfig, SizeShareOverflow, WindowRule, WindowRuleConditions,
};
use crate::model::contexts::{ContextId, ContextKey};
use crate::model::scroll_viewport::ViewportState;
use crate::model::{
    ContainerKind, Direction, LayoutId, LayoutKind, LayoutTree, NodeId, Orientation,
    SpaceLayoutMapping,
};
use crate::sys::geometry::{CGRectDef, CGRectExt, CGSizeExt};
use crate::sys::screen::SpaceId;

#[allow(dead_code)]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum LayoutCommand {
    NextLayout,
    PrevLayout,
    MoveFocus(#[serde(rename = "direction")] Direction),
    Ascend,
    Descend,
    MoveNode(Direction),
    Split(Orientation),
    ToggleOrientation,
    Group(Orientation),
    Ungroup,
    ToggleFocusFloating,
    ToggleWindowFloating,
    ToggleFullscreen,
    Resize {
        #[serde(rename = "direction")]
        direction: Direction,
        #[serde(default = "default_resize_percent")]
        percent: f64,
    },
    CycleColumnWidth,
    ChangeLayoutKind,
    ToggleColumnTabbed,
    FocusNext,
    FocusPrev,
    CleanUpSpace,
    /// Freeze the focused window's share of the tiled area.
    SetSizeShare(SizeShare),
    /// Toggle a size lock at the window's current share.
    ToggleSizeLock,
}

/// A share of the tiled area requested by [`LayoutCommand::SetSizeShare`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(untagged)]
pub enum SizeShare {
    /// A fraction of the tiled area, e.g. `0.5` for half the screen.
    Fraction(f64),
    /// One nth of the tiled area, e.g. `{ denominator = 3 }` for a third.
    Denominator { denominator: u32 },
}

impl SizeShare {
    /// The fraction of the tiled area this share represents, if valid.
    ///
    /// Returns `None` for shares <= 0 or >= 1 (use fullscreen for 100%).
    pub fn fraction(self) -> Option<f64> {
        match self {
            SizeShare::Fraction(share) if share > 0.0 && share < 1.0 => Some(share),
            SizeShare::Denominator { denominator } if denominator >= 2 => {
                Some(1.0 / f64::from(denominator))
            }
            _ => None,
        }
    }
}

/// What happened to a [`LayoutCommand::SetSizeShare`], for UI feedback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizeShareFeedback {
    pub wid: WindowId,
    pub share: f64,
    pub outcome: SizeShareOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeShareOutcome {
    /// The window's estate is now locked at `share`.
    Applied,
    /// The window's estate is no longer locked.
    Released,
    /// The lock was refused because it would cover more than the screen.
    Rejected,
}

fn default_resize_percent() -> f64 {
    5.0
}

#[derive(Debug, Clone, PartialEq)]
pub enum LayoutEvent {
    /// Used during restoration to make sure we don't retain windows for
    /// terminated apps.
    AppsRunningUpdated(HashSet<pid_t>),
    AppClosed(pid_t),
    /// Updates the set of windows for a given app and space.
    WindowsOnScreenUpdated(SpaceId, pid_t, Vec<(WindowId, LayoutWindowInfo)>),
    WindowAdded(SpaceId, WindowId, LayoutWindowInfo),
    WindowRemoved(WindowId),
    NativeTabSelected {
        previous: WindowId,
        selected: WindowId,
        members: Vec<WindowId>,
    },
    WindowSpaceChanged {
        wid: WindowId,
        added: Option<SpaceId>,
        removed: Option<SpaceId>,
        info: LayoutWindowInfo,
        /// Whether contexts are in use. If so, the window leaves every
        /// layout of the Space it left. Otherwise it leaves only the layout
        /// the Space shows.
        contexts_in_use: bool,
    },
    WindowFocused(Vec<SpaceId>, WindowId),
    WindowResized {
        wid: WindowId,
        old_frame: CGRect,
        new_frame: CGRect,
        screens: Vec<(SpaceId, CGRect)>,
    },
    /// A user- or app-originated frame change. Floating windows retain this
    /// frame for their next tiled-to-floating transition.
    WindowFrameChanged {
        wid: WindowId,
        frame: CGRect,
    },
    /// The Space is on screen at this size and shows this context.
    SpaceExposed(SpaceId, CGSize, ActiveContext),
    MouseMovedOverWindow {
        over: (SpaceId, WindowId),
        current_main: Option<(SpaceId, WindowId)>,
    },
}

/// A Space's active context, as the reactor passes it with
/// [`LayoutEvent::SpaceExposed`].
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveContext {
    pub key: ContextKey,
    /// The open windows that are members of `key`. Read only when `key` gets
    /// its first layout on the Space. Empty for Everything.
    pub members: BTreeSet<WindowId>,
}

impl ActiveContext {
    pub const EVERYTHING: ActiveContext = ActiveContext {
        key: ContextKey::Everything,
        members: BTreeSet::new(),
    };
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayoutWindowInfo {
    /// Frame reported before Glide first tiles this window.
    pub frame: CGRect,
    pub bundle_id: Option<String>,
    pub app_name: Option<String>,
    pub title: Option<Secret<String>>,
    pub layer: Option<i32>,
    pub is_standard: bool,
    pub is_resizable: bool,
    pub ax_role: String,
    pub ax_subrole: Option<String>,
}

#[must_use]
#[derive(Debug, Clone, Default)]
pub struct EventResponse {
    /// One-shot frame targets for transitions such as restoring a floating
    /// window. The reactor merges these into its next animation.
    pub frame_overrides: Vec<(WindowId, CGRect)>,
    /// Windows to raise quietly. No WindowFocused events will be created for
    /// these.
    pub raise_windows: Vec<WindowId>,
    /// Window to focus. This window will be raised after the windows in
    /// raise_windows and a WindowFocused event will be generated.
    pub focus_window: Option<WindowId>,
    /// One-shot UI feedback for a size share command, if any.
    pub size_share_feedback: Option<SizeShareFeedback>,
    /// Whether the focused window is floating. Used to update the status bar menu.
    pub focused_window_floating: Option<bool>,
}

impl EventResponse {
    pub fn coalesce(mut self, other: Self) -> Self {
        self.frame_overrides.extend(other.frame_overrides);
        self.raise_windows.extend(other.raise_windows);
        self.size_share_feedback = self.size_share_feedback.or(other.size_share_feedback);
        self.focused_window_floating =
            other.focused_window_floating.or(self.focused_window_floating);
        match (self.focus_window, other.focus_window) {
            (Some(focus_window), Some(other_focus)) => {
                self.focus_window = Some(focus_window);
                self.raise_windows.push(other_focus);
            }
            (None, Some(other_focus)) => {
                self.focus_window = Some(other_focus);
            }
            _ => {}
        }
        self
    }
}

impl LayoutCommand {
    fn modifies_layout(&self) -> bool {
        use LayoutCommand::*;
        match self {
            MoveNode(_)
            | ToggleOrientation
            | Group(_)
            | Ungroup
            | Resize { .. }
            | CycleColumnWidth
            | ToggleColumnTabbed
            | CleanUpSpace => true,

            NextLayout | PrevLayout | MoveFocus(_) | Ascend | Descend | Split(_)
            | ToggleFocusFloating | ToggleWindowFloating | ToggleFullscreen | ChangeLayoutKind
            | FocusNext | FocusPrev | SetSizeShare(_) | ToggleSizeLock => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResizeEdge(u8);

impl ResizeEdge {
    const LEFT: u8 = 0b0001;
    const RIGHT: u8 = 0b0010;
    const TOP: u8 = 0b0100;
    const BOTTOM: u8 = 0b1000;

    fn has_horizontal(self) -> bool {
        self.0 & (Self::LEFT | Self::RIGHT) != 0
    }

    fn has_vertical(self) -> bool {
        self.0 & (Self::TOP | Self::BOTTOM) != 0
    }

    fn is_empty(self) -> bool {
        self.0 == 0
    }
}

struct InteractiveScrollResize {
    column_node: NodeId,
    window_node: NodeId,
    edges: ResizeEdge,
    last_mouse: CGPoint,
}

struct InteractiveScrollMove {
    layout_id: LayoutId,
    window_id: WindowId,
    window_node: NodeId,
    start_mouse: CGPoint,
    drag_active: bool,
}

/// The action that will occur when a drag operation completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropAction {
    /// Swap window assignments between two nodes (safest operation).
    Swap { target_node: NodeId },
    /// Insert the dragged node as a sibling of the target.
    Insert { target_node: NodeId, before: bool },
    /// Create a new container around the target and insert the dragged node.
    Split {
        target_node: NodeId,
        orientation: ContainerKind,
    },
}

/// Update from drag operation for the reactor to handle.
#[derive(Debug)]
pub enum DragUpdate {
    /// Preview frames changed, animate windows to new preview positions.
    PreviewChanged {
        source_node: NodeId,
        action: DropAction,
        preview_frames: Vec<(WindowId, CGRect)>,
    },
    /// No valid drop target, restore windows to original positions.
    RestoreOriginal {
        original_frames: Vec<(WindowId, CGRect)>,
    },
    /// Action unchanged, no update needed.
    NoChange,
}

/// Region within a window frame for drop zone detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropZoneRegion {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

/// State for an active drag-to-rearrange operation.
struct InteractiveDrag {
    layout_id: LayoutId,
    source_wid: WindowId,
    source_node: NodeId,
    start_mouse: CGPoint,
    drag_active: bool,
    hover_target: Option<HoverTarget>,
    current_action: Option<DropAction>,
    /// Preview state for real-time window rearrangement.
    preview: DragPreviewState,
}

/// Information about the window currently being hovered during drag.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields kept for future drop zone overlay generation
struct HoverTarget {
    node: NodeId,
    wid: WindowId,
    frame: CGRect,
    zone: DropZoneRegion,
}

/// State for real-time preview of window positions during drag.
struct DragPreviewState {
    /// Original window frames before drag began (for restoration).
    original_frames: HashMap<WindowId, CGRect>,
    /// The last action that was applied as a preview.
    last_action: Option<DropAction>,
}

const RESIZE_EDGE_THRESHOLD: f64 = 8.0;
const MOVE_DRAG_THRESHOLD: f64 = 10.0;

impl DropZoneRegion {
    /// Compute which zone a point falls into within a frame.
    ///
    /// Zone sizes:
    /// - Left/Right (insert zones): use `edge_ratio` (default 15%)
    /// - Bottom (horizontal split zone): 1/3 of height
    /// - Top zone is disabled (resolves to center or left/right at corners)
    fn from_point(point: CGPoint, frame: CGRect, edge_ratio: f64) -> Self {
        let rel_x = (point.x - frame.origin.x) / frame.size.width;
        let rel_y = (point.y - frame.origin.y) / frame.size.height;

        // Left/right use edge_ratio, bottom uses 1/3 for horizontal split
        const BOTTOM_ZONE_RATIO: f64 = 1.0 / 3.0;

        let in_left = rel_x < edge_ratio;
        let in_right = rel_x > (1.0 - edge_ratio);
        let in_top = rel_y < edge_ratio;
        let in_bottom = rel_y > (1.0 - BOTTOM_ZONE_RATIO);

        // Corners: pick the edge we're deeper into
        // Top zone is disabled - corners resolve to left/right, top edge resolves to center
        match (in_left, in_right, in_top, in_bottom) {
            // Top-left corner: always pick left (top zone disabled)
            (true, _, true, _) => DropZoneRegion::Left,
            // Bottom-left corner: compare depth into each zone
            (true, _, _, true) => {
                // Depth into left zone (from left edge)
                let left_depth = rel_x / edge_ratio;
                // Depth into bottom zone (from bottom edge)
                let bottom_depth = (rel_y - (1.0 - BOTTOM_ZONE_RATIO)) / BOTTOM_ZONE_RATIO;
                if left_depth < bottom_depth {
                    DropZoneRegion::Left
                } else {
                    DropZoneRegion::Bottom
                }
            }
            // Top-right corner: always pick right (top zone disabled)
            (_, true, true, _) => DropZoneRegion::Right,
            // Bottom-right corner: compare depth into each zone
            (_, true, _, true) => {
                // Depth into right zone (from right edge)
                let right_depth = (rel_x - (1.0 - edge_ratio)) / edge_ratio;
                // Depth into bottom zone (from bottom edge)
                let bottom_depth = (rel_y - (1.0 - BOTTOM_ZONE_RATIO)) / BOTTOM_ZONE_RATIO;
                if right_depth < bottom_depth {
                    DropZoneRegion::Right
                } else {
                    DropZoneRegion::Bottom
                }
            }
            (true, ..) => DropZoneRegion::Left,
            (_, true, ..) => DropZoneRegion::Right,
            // Top edge (not in corners) falls through to center
            (.., true) => DropZoneRegion::Bottom,
            _ => DropZoneRegion::Center,
        }
    }

    /// Convert a zone to a drop action based on parent container orientation.
    ///
    /// - Center always maps to Swap.
    /// - Edges along the parent's orientation map to Insert.
    /// - Edges perpendicular to parent's orientation map to Split.
    fn to_action(self, target_node: NodeId, parent_orientation: Orientation) -> DropAction {
        match self {
            DropZoneRegion::Center => DropAction::Swap { target_node },
            DropZoneRegion::Left | DropZoneRegion::Right => {
                let before = self == DropZoneRegion::Left;
                if parent_orientation == Orientation::Horizontal {
                    DropAction::Insert { target_node, before }
                } else {
                    DropAction::Split {
                        target_node,
                        orientation: ContainerKind::Horizontal,
                    }
                }
            }
            DropZoneRegion::Top | DropZoneRegion::Bottom => {
                let before = self == DropZoneRegion::Top;
                if parent_orientation == Orientation::Vertical {
                    DropAction::Insert { target_node, before }
                } else {
                    DropAction::Split {
                        target_node,
                        orientation: ContainerKind::Vertical,
                    }
                }
            }
        }
    }
}

/// Actor that manages the layouts for each space.
///
/// The LayoutManager is the event-driven layer that sits between the Reactor
/// and the LayoutTree model. This actor receives commands and (cleaned up)
/// events from the Reactor, converts them into LayoutTree operations, and
/// calculates the desired position and size of each window. It also manages
/// floating windows.
///
/// LayoutManager keeps a different layout for each screen size a space is used
/// on. See [`SpaceLayoutInfo`] for more details.
//
// TODO: LayoutManager has too many roles. Consider splitting into a few layers:
//
// * Restoration and new/removed windows/apps.
//   * Convert WindowsOnScreenUpdated events into adds/removes.
// * (Virtual workspaces could go around here.)
// * Floating/tiling split.
// * Tiling layout selection (manual and automatic based on size).
// * Tiling layout-specific commands.
//
// If glide core had a true public API I'd expect it to go after the restoration
// or virtual workspaces layer.
#[derive(Serialize, Deserialize)]
pub struct LayoutManager {
    tree: LayoutTree,
    /// Everything's layouts on each Space.
    layout_mapping: HashMap<SpaceId, SpaceLayoutMapping>,
    /// The layouts of each other context on each Space.
    #[serde(default, with = "context_layouts_serde")]
    context_layouts: HashMap<(SpaceId, ContextKey), SpaceLayoutMapping>,
    /// The active context of each Space, from the last `SpaceExposed`. A Space
    /// without an entry shows Everything.
    #[serde(skip)]
    active_contexts: HashMap<SpaceId, ContextKey>,
    floating_windows: BTreeSet<WindowId>,
    /// The last user-controlled frame for each floating window. This is kept
    /// after a window is tiled so toggling it back to floating can restore it.
    #[serde(default)]
    floating_restore_frames: HashMap<WindowId, FloatingRestoreFrame>,
    #[serde(skip)]
    active_floating_windows: ActiveFloatingWindows,
    #[serde(skip)]
    focused_window: Option<WindowId>,
    /// Last window focused in floating mode.
    #[serde(skip)]
    // TODO: We should keep a stack for each space.
    last_floating_focus: Option<WindowId>,
    #[serde(skip)]
    viewports: HashMap<LayoutId, ViewportState>,
    #[serde(skip)]
    default_layout_kind: LayoutKind,
    #[serde(skip)]
    scroll_cfg: ScrollConfig,
    #[serde(skip)]
    scroll_enabled: bool,
    #[serde(skip)]
    window_rules: Vec<WindowRule>,
    #[serde(skip)]
    interactive_resize: Option<InteractiveScrollResize>,
    #[serde(skip)]
    interactive_move: Option<InteractiveScrollMove>,
    #[serde(skip)]
    interactive_drag: Option<InteractiveDrag>,
    #[serde(skip, default = "default_config")]
    config: Arc<Config>,
}

#[derive(Serialize, Deserialize)]
struct FloatingRestoreFrame {
    #[serde(with = "CGRectDef")]
    frame: CGRect,
}

/// How `layout.ron` writes a [`ContextKey`].
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash)]
enum SavedContextKey {
    Everything,
    Unsorted,
    Named(ContextId),
}

impl From<ContextKey> for SavedContextKey {
    fn from(key: ContextKey) -> Self {
        match key {
            ContextKey::Everything => SavedContextKey::Everything,
            ContextKey::Unsorted => SavedContextKey::Unsorted,
            ContextKey::Named(id) => SavedContextKey::Named(id),
        }
    }
}

impl From<SavedContextKey> for ContextKey {
    fn from(key: SavedContextKey) -> Self {
        match key {
            SavedContextKey::Everything => ContextKey::Everything,
            SavedContextKey::Unsorted => ContextKey::Unsorted,
            SavedContextKey::Named(id) => ContextKey::Named(id),
        }
    }
}

mod context_layouts_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::SavedContextKey;
    use crate::collections::HashMap;
    use crate::model::SpaceLayoutMapping;
    use crate::model::contexts::ContextKey;
    use crate::sys::screen::SpaceId;

    type ContextLayouts = HashMap<(SpaceId, ContextKey), SpaceLayoutMapping>;

    pub fn serialize<S: Serializer>(
        map: &ContextLayouts,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_map(
            map.iter()
                .map(|(&(space, key), mapping)| ((space, SavedContextKey::from(key)), mapping)),
        )
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ContextLayouts, D::Error> {
        let saved =
            HashMap::<(SpaceId, SavedContextKey), SpaceLayoutMapping>::deserialize(deserializer)?;
        Ok(saved
            .into_iter()
            .map(|((space, key), mapping)| ((space, key.into()), mapping))
            .collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum WindowClass {
    Untracked,
    FloatByDefault,
    Regular,
}

/// Returns true if every specified condition of `rule` matches the window. A
/// rule with no conditions matches every window.
fn window_rule_matches(conditions: &WindowRuleConditions, info: &LayoutWindowInfo) -> bool {
    let title = info.title.as_ref().map(|t| t.expose_secret().as_str());
    let eq = |pattern: Option<&str>, value: Option<&str>| {
        pattern.map_or(true, |p| value.is_some_and(|v| v.eq_ignore_ascii_case(p)))
    };
    let contains = |needle: Option<&str>, haystack: Option<&str>| {
        needle.map_or(true, |n| {
            haystack.is_some_and(|v| v.to_lowercase().contains(&n.to_lowercase()))
        })
    };
    let matches = |re: Option<&regex::Regex>, value: Option<&str>| {
        re.map_or(true, |re| value.is_some_and(|v| re.is_match(v)))
    };
    eq(conditions.app_id.as_deref(), info.bundle_id.as_deref())
        && contains(conditions.app_name.as_deref(), info.app_name.as_deref())
        && matches(conditions.title_regex.as_deref(), title)
        && contains(conditions.title_substring.as_deref(), title)
        && eq(conditions.ax_role.as_deref(), Some(info.ax_role.as_str()))
        && eq(conditions.ax_subrole.as_deref(), info.ax_subrole.as_deref())
}

fn classify_window(rules: &[WindowRule], info: &LayoutWindowInfo) -> WindowClass {
    use LayoutWindowInfo as Info;

    // Phantom/non-window cases are handled first and can't be overridden by app
    // rules, since floating or tiling a window that doesn't really exist makes
    // no sense.
    match info {
        &Info { layer: Some(layer), .. } if layer != 0 => return WindowClass::Untracked,

        // Finder reports a nonstandard window that doesn't actually "exist".
        // In general windows with no layer info are suspect, since it means
        // we couldn't find a corresponding window server window, but we try
        // not to lean on this too much since it depends on a private API.
        Info {
            layer: None,
            is_standard: false,
            bundle_id: Some(bundle_id),
            ..
        } if bundle_id == "com.apple.finder" => return WindowClass::Untracked,

        // Firefox picture-in-picture windows sometimes get observed at layer 0
        // after they are created, even though the layer is later changed to 3.
        // We don't have an event source for the layer change so special case
        // them here. #154
        Info {
            title: Some(title),
            bundle_id: Some(bundle_id),
            ..
        } if bundle_id == "org.mozilla.firefox"
            && title.expose_secret() == "Picture-in-Picture" =>
        {
            return WindowClass::Untracked;
        }

        _ => {}
    }

    // The first matching user rule overrides the built-in heuristics below.
    if let Some(rule) = rules.iter().find(|rule| window_rule_matches(&rule.conditions, info)) {
        return if rule.float {
            WindowClass::FloatByDefault
        } else {
            WindowClass::Regular
        };
    }

    match info {
        Info { is_standard: false, .. } => WindowClass::FloatByDefault,
        Info { is_resizable: false, .. } => WindowClass::FloatByDefault,

        // Float system preferences windows, since they don't resize horiztonally.
        Info { bundle_id: Some(bundle_id), .. } if bundle_id == "com.apple.systempreferences" => {
            WindowClass::FloatByDefault
        }

        _ => WindowClass::Regular,
    }
}

fn default_config() -> Arc<Config> {
    Arc::new(Config::default())
}

impl LayoutManager {
    pub fn new(config: Arc<Config>) -> Self {
        LayoutManager {
            tree: LayoutTree::new(),
            layout_mapping: Default::default(),
            context_layouts: Default::default(),
            active_contexts: Default::default(),
            floating_windows: Default::default(),
            floating_restore_frames: Default::default(),
            active_floating_windows: Default::default(),
            focused_window: None,
            last_floating_focus: None,
            viewports: Default::default(),
            default_layout_kind: LayoutKind::default(),
            scroll_cfg: Config::default().settings.experimental.scroll.validated(),
            scroll_enabled: false,
            window_rules: Vec::new(),
            interactive_resize: None,
            interactive_move: None,
            interactive_drag: None,
            config,
        }
    }

    pub fn set_config(&mut self, config: &Arc<Config>) {
        // TODO: read these through self.config instead of cloning them out
        self.config = config.clone();
        self.scroll_cfg = config.settings.experimental.scroll.clone().validated();
        self.scroll_enabled = self.scroll_cfg.enable;
        self.window_rules = config.window_rules.clone();
        self.default_layout_kind = match (self.scroll_enabled, config.settings.default_layout_kind)
        {
            (false, LayoutKind::Scroll) => {
                warn!(
                    "Ignoring default_layout_kind=scroll because experimental.scroll.enable=false"
                );
                LayoutKind::Tree
            }
            (_, kind) => kind,
        };
        if !self.scroll_enabled {
            self.convert_active_scroll_layouts_to_tree();
        }
    }

    fn convert_active_scroll_layouts_to_tree(&mut self) {
        let mappings = self
            .layout_mapping
            .keys()
            .map(|&space| (space, ContextKey::Everything))
            .chain(self.context_layouts.keys().copied())
            .collect::<Vec<_>>();
        for (space, key) in mappings {
            self.ensure_layout_kind_allowed(space, key);
        }
    }

    /// Converts the active layout of the context's mapping on the Space to a
    /// tree layout when scroll layouts are disabled.
    fn ensure_layout_kind_allowed(&mut self, space: SpaceId, key: ContextKey) {
        if self.scroll_enabled {
            return;
        }
        let Some(mapping) = self.mapping(space, key) else {
            return;
        };
        let layout = mapping.active_layout();
        if !self.tree.is_scroll_layout(layout) {
            return;
        }
        debug!(
            ?space,
            ?key,
            "Converting scroll layout to tree because scroll gate is disabled"
        );
        let new_layout = Self::convert_layout_kind(
            &mut self.tree,
            &self.scroll_cfg,
            self.focused_window,
            layout,
            LayoutKind::Tree,
        );
        if let Some((mapping, _)) = self.mapping_mut(space, key)
            && mapping.active_layout() == layout
        {
            mapping.replace_active_layout(new_layout);
        }
        self.viewports.remove(&layout);
    }

    fn convert_layout_kind(
        tree: &mut LayoutTree,
        scroll_cfg: &ScrollConfig,
        focused_window: Option<WindowId>,
        layout: LayoutId,
        new_kind: LayoutKind,
    ) -> LayoutId {
        if tree.layout_kind(layout) == new_kind {
            return layout;
        }

        let selected_window = tree.window_at(tree.selection(layout));
        let windows: Vec<WindowId> = tree
            .root(layout)
            .traverse_postorder(tree.map())
            .filter_map(|n| tree.window_at(n))
            .collect();

        let new_layout = match new_kind {
            LayoutKind::Tree => tree.create_layout(),
            LayoutKind::Scroll => tree.create_scroll_layout(),
        };

        let visible_columns = scroll_cfg.visible_columns;
        for wid in windows {
            tree.remove_window_from(layout, wid);
            if new_kind == LayoutKind::Scroll {
                tree.add_window_to_scroll_column_with_visible(
                    new_layout,
                    wid,
                    true,
                    visible_columns,
                );
            } else {
                let sel = tree.selection(new_layout);
                tree.add_window_after(new_layout, sel, wid);
            }
        }

        if let Some(wid) = focused_window.or(selected_window)
            && let Some(node) = tree.window_node(new_layout, wid)
        {
            tree.select(node);
        }

        new_layout
    }

    fn change_layout_index_filtered(
        mapping: &mut SpaceLayoutMapping,
        tree: &LayoutTree,
        offset: i16,
        allow_scroll: bool,
    ) -> LayoutId {
        let layouts: Vec<_> = mapping.layouts().collect();
        let len = layouts.len();
        if len <= 1 {
            return mapping.active_layout();
        }
        let cur_idx = mapping.active_layout_index() as i16;
        for step in 1..=len {
            let idx = (cur_idx + offset * step as i16).rem_euclid(len as i16) as usize;
            let candidate = layouts[idx];
            if allow_scroll || !tree.is_scroll_layout(candidate) {
                mapping.select_layout(candidate);
                return candidate;
            }
        }
        mapping.active_layout()
    }

    /// Selects the layout `offset` steps away in the Space's active mapping.
    /// Does nothing while a context is active on the Space.
    fn change_layout_index(&mut self, space: SpaceId, offset: i16) -> EventResponse {
        if self.active_context(space) != ContextKey::Everything {
            return EventResponse::default();
        }
        let allow_scroll = self.scroll_enabled;
        let Some((mapping, tree)) = self.active_mapping_mut(space) else {
            return EventResponse::default();
        };
        // FIXME: Update windows in the new layout.
        let layout = Self::change_layout_index_filtered(mapping, tree, offset, allow_scroll);
        if let Some(wid) = self.focused_window
            && let Some(node) = self.tree.window_node(layout, wid)
        {
            self.tree.select(node);
        }
        EventResponse::default()
    }

    pub fn debug_tree(&self, space: SpaceId) {
        self.debug_tree_desc(space, "", false);
    }

    pub fn debug_tree_desc(&self, space: SpaceId, desc: &'static str, print: bool) {
        macro_rules! log {
            ($print:expr, $($args:tt)*) => {
                if $print {
                    tracing::warn!($($args)*);
                } else {
                    tracing::debug!($($args)*);
                }
            };
        }
        if let Some(layout) = self.try_layout(space) {
            log!(print, "Tree {desc}\n{}", self.tree.draw_tree(layout).trim());
        } else {
            log!(print, "No layout for space {space:?}");
        }
        let floating = self.active_floating_windows.in_space(space).collect::<Vec<_>>();
        if !floating.is_empty() {
            log!(print, "Floating {floating:?}");
        }
    }

    pub fn handle_event(&mut self, event: LayoutEvent) -> EventResponse {
        debug!(?event);
        match event {
            LayoutEvent::SpaceExposed(space, size, context) => {
                self.debug_tree(space);
                let kind = self.default_layout_kind;
                let mapping = self
                    .layout_mapping
                    .entry(space)
                    .or_insert_with(|| SpaceLayoutMapping::new(size, &mut self.tree, kind));
                mapping.activate_size(size, &mut self.tree);
                if context.key != ContextKey::Everything {
                    if !self.context_layouts.contains_key(&(space, context.key)) {
                        self.create_context_mapping(space, size, &context);
                    }
                    if let Some((mapping, tree)) = self.mapping_mut(space, context.key) {
                        mapping.activate_size(size, tree);
                    }
                }
                self.active_contexts.insert(space, context.key);
                self.ensure_layout_kind_allowed(space, self.shown_context(space));
                return EventResponse {
                    frame_overrides: vec![],
                    raise_windows: self.top_layer_windows(space),
                    focus_window: None,
                    ..Default::default()
                };
            }
            LayoutEvent::WindowsOnScreenUpdated(space, pid, mut windows) => {
                self.debug_tree(space);
                // Sort windows by x position so they're added to the tree in spatial order.
                // This prevents reshuffling windows that are already arranged correctly.
                windows.sort_by(|(_, a), (_, b)| {
                    a.frame
                        .origin
                        .x
                        .partial_cmp(&b.frame.origin.x)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });

                // The windows may already be in the layout if we restored a saved state, so
                // make sure not to duplicate or erase them here.
                for (wid, info) in &windows {
                    self.floating_restore_frames
                        .entry(*wid)
                        .or_insert(FloatingRestoreFrame { frame: info.frame });
                }
                let window_map = windows.iter().cloned().collect::<HashMap<_, _>>();
                self.last_floating_focus
                    .take_if(|f| f.pid == pid && !window_map.contains_key(f));
                let layout = self.layout(space);
                let floating_active = self.active_floating_windows.reset_app(space, pid);
                let mut add_floating = Vec::new();
                let mut new_windows = Vec::new();
                let mut has_new_tree_windows = false;
                let tree_windows = windows
                    .iter()
                    .map(|(wid, _info)| *wid)
                    .filter(|wid| {
                        let floating = self.floating_windows.contains(wid);
                        if floating {
                            floating_active.insert(*wid);
                            return false;
                        }
                        if self.tree.window_node(layout, *wid).is_some() {
                            return true;
                        }
                        match classify_window(&self.window_rules, window_map.get(wid).unwrap()) {
                            WindowClass::Untracked => false,
                            WindowClass::FloatByDefault => {
                                add_floating.push(*wid);
                                false
                            }
                            WindowClass::Regular => {
                                if self.tree.is_scroll_layout(layout) {
                                    new_windows.push(*wid);
                                    false
                                } else {
                                    has_new_tree_windows = true;
                                    true
                                }
                            }
                        }
                    })
                    .collect();
                self.tree.set_windows_for_app(self.layout(space), pid, tree_windows);

                // Reorder columns to match actual window positions on screen.
                // Only do this when new windows are added, not when returning to
                // a space where windows are already positioned correctly.
                if !self.tree.is_scroll_layout(layout) && has_new_tree_windows {
                    self.reorder_columns_by_position(layout);
                }

                let has_new_scroll_windows = !new_windows.is_empty();
                for wid in new_windows {
                    self.add_scroll_window(layout, wid);
                }

                // For scroll layouts, reorder columns after adding windows to match
                // their actual screen positions. This prevents shuffling windows
                // that are already arranged correctly (e.g., on startup).
                if self.tree.is_scroll_layout(layout) && has_new_scroll_windows {
                    self.reorder_columns_by_position(layout);
                }
                for wid in add_floating {
                    self.add_floating_window(wid, Some(space));
                }
            }
            LayoutEvent::AppsRunningUpdated(hash_set) => {
                self.tree.retain_apps(|pid| hash_set.contains(&pid));
                self.tree.retain_size_locks(|wid| hash_set.contains(&wid.pid));
                self.floating_restore_frames.retain(|wid, _| hash_set.contains(&wid.pid));
            }
            LayoutEvent::AppClosed(pid) => {
                self.tree.remove_windows_for_app(pid);
                self.tree.retain_size_locks(|wid| wid.pid != pid);
                self.floating_windows.remove_all_for_pid(pid);
                self.floating_restore_frames.retain(|wid, _| wid.pid != pid);
            }
            LayoutEvent::WindowAdded(space, wid, info) => {
                self.debug_tree(space);
                self.floating_restore_frames
                    .entry(wid)
                    .or_insert(FloatingRestoreFrame { frame: info.frame });
                if self.tree.window_node(self.layout(space), wid).is_some() {
                    // A new window reaches the layout through the window list
                    // too, which may have added it already.
                    return EventResponse::default();
                }
                match classify_window(&self.window_rules, &info) {
                    WindowClass::FloatByDefault => self.add_floating_window(wid, Some(space)),
                    WindowClass::Regular => {
                        let layout = self.layout(space);
                        if self.tree.is_scroll_layout(layout) {
                            self.add_scroll_window(layout, wid);
                        } else {
                            self.tree.add_window_after(layout, self.tree.selection(layout), wid);
                        }
                    }
                    WindowClass::Untracked => (),
                }
            }
            LayoutEvent::NativeTabSelected { previous, selected, members } => {
                self.cancel_interactive_state();
                for &member in &members {
                    if member != previous && member != selected {
                        self.tree.remove_window(member);
                    }
                }
                self.tree.replace_window(previous, selected);
                let floating = self.floating_windows.contains(&previous);
                if floating {
                    self.floating_windows.insert(selected);
                } else {
                    self.floating_windows.remove(&selected);
                }
                if floating {
                    let spaces: Vec<_> = self
                        .active_floating_windows
                        .by_space
                        .keys()
                        .copied()
                        .filter(|&space| {
                            self.active_floating_windows.in_space(space).any(|wid| wid == previous)
                        })
                        .collect();
                    for space in spaces {
                        self.remove_window_from_shown_context(selected, Some(space));
                    }
                }
                if let Some(restore) = self.floating_restore_frames.get(&previous) {
                    let frame = restore.frame;
                    self.floating_restore_frames.insert(selected, FloatingRestoreFrame { frame });
                }
                self.active_floating_windows.select_tab(previous, selected, &members, floating);
                if self.focused_window == Some(previous) {
                    self.focused_window = Some(selected);
                }
                if self.last_floating_focus == Some(previous) {
                    self.last_floating_focus = Some(selected);
                }
            }
            LayoutEvent::WindowRemoved(wid) => {
                self.tree.remove_window(wid);
                self.tree.clear_size_lock(wid);
                self.floating_windows.remove(&wid);
                self.floating_restore_frames.remove(&wid);
            }
            LayoutEvent::WindowFrameChanged { wid, frame } => {
                if self.floating_windows.contains(&wid) {
                    self.floating_restore_frames.insert(wid, FloatingRestoreFrame { frame });
                }
            }
            LayoutEvent::WindowSpaceChanged {
                wid,
                added,
                removed,
                info,
                contexts_in_use,
            } => {
                if self.floating_windows.contains(&wid) {
                    // Floating windows live outside the tree, tracked per space
                    // in active_floating_windows.
                    if let Some(added) = added {
                        self.add_floating_window(wid, Some(added));
                    }
                    if let Some(removed) = removed {
                        self.active_floating_windows.remove(removed, wid);
                    }
                } else {
                    if let Some(added) = added {
                        let class = if self.tree.has_window(wid) {
                            // A window we already tile keeps its placement.
                            WindowClass::Regular
                        } else {
                            // We may not have seen this window before, which can
                            // happen if it was off screen before.
                            classify_window(&self.window_rules, &info)
                        };
                        match class {
                            WindowClass::Untracked => (),
                            WindowClass::FloatByDefault => {
                                self.add_floating_window(wid, Some(added))
                            }
                            WindowClass::Regular => {
                                let layout = self.layout(added);
                                if self.tree.is_scroll_layout(layout) {
                                    self.add_scroll_window(layout, wid);
                                } else {
                                    self.tree.add_window_after(
                                        layout,
                                        self.tree.selection(layout),
                                        wid,
                                    );
                                }
                            }
                        }
                    }
                    if let Some(removed) = removed {
                        self.remove_window_from_space(removed, wid, contexts_in_use);
                    }
                }
            }
            LayoutEvent::WindowFocused(spaces, wid) => {
                self.focused_window = Some(wid);
                let is_floating = self.floating_windows.contains(&wid);
                if is_floating {
                    self.last_floating_focus = Some(wid);
                } else {
                    for space in &spaces {
                        self.clear_user_scrolling(*space);
                    }
                    for space in spaces {
                        let layout = self.layout(space);
                        if let Some(node) = self.tree.window_node(layout, wid) {
                            self.tree.select(node);
                        }
                    }
                }
                return EventResponse {
                    focused_window_floating: Some(is_floating),
                    ..Default::default()
                };
            }
            LayoutEvent::WindowResized {
                wid,
                old_frame,
                new_frame,
                screens,
            } => {
                for (space, screen) in screens {
                    let layout = self.layout(space);
                    let Some(node) = self.tree.window_node(layout, wid) else {
                        continue;
                    };
                    if !screen.size.contains(old_frame.size)
                        || !screen.size.contains(new_frame.size)
                    {
                        // Ignore resizes involving sizes outside the normal
                        // screen bounds. This can happen for instance if the
                        // window becomes fullscreen at the system level.
                        debug!("Ignoring out-of-bounds resize");
                        continue;
                    }
                    if new_frame == screen {
                        // Usually this happens because the user double-clicked
                        // the title bar.
                        self.tree.set_fullscreen(node, true);
                    } else if self.tree.is_fullscreen(node) {
                        // Either the user double-clicked the window to restore
                        // it from full-size, or they are in an interactive
                        // resize. In both cases we should ignore the old_frame
                        // because it does not reflect the layout size in the
                        // tree (fullscreen overrides that). In the interactive
                        // case clearing the fullscreen bit will cause us to
                        // resize the window to our expected restore size, and
                        // the next resize event we see from the user will
                        // correctly use that as the old_frame.
                        self.tree.set_fullscreen(node, false);
                    } else {
                        // n.b.: old_frame should reflect the current size in
                        // the layout tree so it can be accurately updated.
                        self.tree.set_frame_from_resize(
                            node,
                            old_frame,
                            new_frame,
                            screen,
                            &self.config,
                        );
                    }
                }
            }
            LayoutEvent::MouseMovedOverWindow {
                over: (new_space, new_wid),
                current_main,
            } => {
                if let Some((cur_space, cur_wid)) = current_main
                    // If either window isn't in the layout at all, ignore. Only
                    // follow the mouse between tiled windows.
                    && let Some(_) = self.tree.window_node(self.layout(cur_space), cur_wid)
                    && let Some(new_node) =
                        self.tree.window_node(self.layout(new_space), new_wid)
                    // Don't follow the mouse to windows that aren't visible
                    // according to the layout. This can happen if there are gaps
                    // between windows or the occluding windows have different
                    // border shapes.
                    && self.tree.is_visible(new_node)
                {
                    return EventResponse {
                        frame_overrides: vec![],
                        raise_windows: vec![],
                        focus_window: Some(new_wid),
                        ..Default::default()
                    };
                }
            }
        }
        EventResponse::default()
    }

    pub fn handle_command(
        &mut self,
        space: Option<SpaceId>,
        visible_spaces: &[SpaceId],
        command: LayoutCommand,
    ) -> EventResponse {
        if let Some(space) = space {
            let layout = self.layout(space);
            debug!("Tree:\n{}", self.tree.draw_tree(layout).trim());
            debug!(selection = ?self.tree.selection(layout));
        }
        let is_floating = self.is_floating();
        debug!(?self.floating_windows);
        debug!(?self.focused_window, ?self.last_floating_focus, ?is_floating);

        if !self.scroll_enabled
            && matches!(
                command,
                LayoutCommand::CycleColumnWidth
                    | LayoutCommand::ToggleColumnTabbed
                    | LayoutCommand::ChangeLayoutKind
            )
        {
            warn!("Ignoring {command:?} because scroll layout is disabled");
            return EventResponse::default();
        }

        // ToggleWindowFloating is the only command that works when the space is
        // disabled.
        if let LayoutCommand::ToggleWindowFloating = &command {
            let Some(wid) = self.focused_window else {
                return EventResponse::default();
            };
            if is_floating {
                self.remove_floating_window(wid, space);
                self.last_floating_focus = None;
                return EventResponse {
                    focused_window_floating: Some(false),
                    ..Default::default()
                };
            } else {
                self.add_floating_window(wid, space);
                self.remove_window_from_shown_context(wid, space);
                self.last_floating_focus = Some(wid);
                return EventResponse {
                    frame_overrides: self
                        .floating_restore_frames
                        .get(&wid)
                        .map(|restore| vec![(wid, restore.frame)])
                        .unwrap_or_default(),
                    focused_window_floating: Some(true),
                    ..Default::default()
                };
            }
        }

        let Some(space) = space else {
            return EventResponse::default();
        };
        let Some((mapping, tree)) = self.active_mapping_mut(space) else {
            error!(
                ?command, ?self.layout_mapping,
                "Could not find layout mapping for current space");
            return EventResponse::default();
        };
        if command.modifies_layout() {
            mapping.prepare_modify(tree);
        }
        let layout = mapping.active_layout();

        if let LayoutCommand::ToggleFocusFloating = &command {
            if is_floating {
                let selection = self.tree.window_at(self.tree.selection(layout));
                let mut raise_windows = self.tree.visible_windows_under(self.tree.root(layout));
                // We need to focus some window to transition into floating
                // mode. If there is no selection, pick a window.
                let focus_window = selection.or_else(|| raise_windows.pop());
                return EventResponse {
                    frame_overrides: vec![],
                    raise_windows,
                    focus_window,
                    // Focusing a tiled window means the focused window is no longer floating.
                    focused_window_floating: Some(false),
                    ..Default::default()
                };
            } else {
                let mut raise_windows: Vec<_> = self
                    .active_floating_windows
                    .in_space(space)
                    .filter(|&wid| Some(wid) != self.last_floating_focus)
                    .collect();
                // We need to focus some window to transition into floating
                // mode. If there is no last floating window, pick one.
                let focus_window = self.last_floating_focus.or_else(|| raise_windows.pop());
                return EventResponse {
                    frame_overrides: vec![],
                    raise_windows,
                    focus_window,
                    // Focusing a floating window means the focused window is now floating.
                    focused_window_floating: Some(true),
                    ..Default::default()
                };
            }
        }

        // Remaining commands only work for tiling layout.
        if is_floating {
            return EventResponse::default();
        }

        let next_space = |direction| {
            // Pick another space based on the order in visible_spaces.
            if visible_spaces.len() <= 1 {
                return None;
            }
            let idx = visible_spaces.iter().enumerate().find(|(_, s)| **s == space)?.0;
            let idx = match direction {
                Direction::Left | Direction::Up => idx as i32 - 1,
                Direction::Right | Direction::Down => idx as i32 + 1,
            };
            let idx = idx.rem_euclid(visible_spaces.len() as i32);
            Some(visible_spaces[idx as usize])
        };

        match command {
            // Handled above.
            LayoutCommand::ToggleWindowFloating => unreachable!(),
            LayoutCommand::ToggleFocusFloating => unreachable!(),

            LayoutCommand::NextLayout => self.change_layout_index(space, 1),
            LayoutCommand::PrevLayout => self.change_layout_index(space, -1),
            LayoutCommand::MoveFocus(direction) => {
                let is_scroll = self.tree.is_scroll_layout(layout);
                let use_wrapping = self.scroll_enabled
                    && is_scroll
                    && self.scroll_config().infinite_loop
                    && matches!(direction, Direction::Left | Direction::Right);
                let new_focus = if use_wrapping {
                    self.tree.traverse_scroll_wrapping(
                        layout,
                        self.tree.selection(layout),
                        direction,
                    )
                } else {
                    self.tree.traverse(self.tree.selection(layout), direction)
                }
                .or_else(|| {
                    let layout = self.layout(next_space(direction)?);
                    Some(self.tree.selection(layout))
                });
                if new_focus.is_some() && is_scroll {
                    self.clear_user_scrolling(space);
                }
                let focus_window = new_focus.and_then(|new| self.tree.window_at(new));
                let raise_windows = new_focus
                    .map(|new| self.tree.select_returning_surfaced_windows(new))
                    .unwrap_or_default();
                EventResponse {
                    frame_overrides: vec![],
                    focus_window,
                    raise_windows,
                    ..Default::default()
                }
            }
            LayoutCommand::FocusNext => {
                let new_focus = self.tree.focus_next(layout, self.tree.selection(layout));
                let focus_window = new_focus.and_then(|new| self.tree.window_at(new));
                let raise_windows = new_focus
                    .map(|new| self.tree.select_returning_surfaced_windows(new))
                    .unwrap_or_default();
                EventResponse {
                    frame_overrides: vec![],
                    focus_window,
                    raise_windows,
                    ..Default::default()
                }
            }
            LayoutCommand::FocusPrev => {
                let new_focus = self.tree.focus_prev(layout, self.tree.selection(layout));
                let focus_window = new_focus.and_then(|new| self.tree.window_at(new));
                let raise_windows = new_focus
                    .map(|new| self.tree.select_returning_surfaced_windows(new))
                    .unwrap_or_default();
                EventResponse {
                    frame_overrides: vec![],
                    focus_window,
                    raise_windows,
                    ..Default::default()
                }
            }
            LayoutCommand::Ascend => {
                self.tree.ascend_selection(layout);
                EventResponse::default()
            }
            LayoutCommand::Descend => {
                self.tree.descend_selection(layout);
                EventResponse::default()
            }
            LayoutCommand::MoveNode(direction) => {
                let selection = self.tree.selection(layout);
                if !self.tree.move_node(layout, selection, direction) {
                    if let Some(new_space) = next_space(direction) {
                        let new_layout = self.layout(new_space);
                        self.tree.move_node_after(self.tree.selection(new_layout), selection);
                    }
                }
                EventResponse::default()
            }
            LayoutCommand::Split(orientation) => {
                let selection = self.tree.selection(layout);
                let map = self.tree.map();
                // Check if there's exactly one sibling - if so, pull it into the
                // new container for an immediate visual rearrangement.
                let prev = selection.prev_sibling(map);
                let next = selection.next_sibling(map);
                let only_sibling = match (prev, next) {
                    (Some(p), None) => Some(p),
                    (None, Some(n)) => Some(n),
                    _ => None, // 0 or 2+ siblings: don't auto-pull
                };
                // Create a container around the selected node
                self.tree.nest_in_container(layout, selection, ContainerKind::from(orientation));
                // Move the only sibling into the new container (after the selected node)
                if let Some(sibling) = only_sibling {
                    self.tree.move_node_after(selection, sibling);
                }
                EventResponse::default()
            }
            LayoutCommand::ToggleOrientation => {
                if let Some(parent) = self.tree.selection(layout).parent(self.tree.map()) {
                    let kind = self.tree.container_kind(parent);
                    self.tree.set_container_kind(parent, kind.flip());
                }
                EventResponse::default()
            }
            LayoutCommand::Group(orientation) => {
                if let Some(parent) = self.tree.selection(layout).parent(self.tree.map()) {
                    self.tree.set_container_kind(parent, ContainerKind::group(orientation));
                }
                EventResponse::default()
            }
            LayoutCommand::Ungroup => {
                if let Some(parent) = self.tree.selection(layout).parent(self.tree.map()) {
                    if self.tree.container_kind(parent).is_group() {
                        self.tree.set_container_kind(
                            parent,
                            self.tree.last_ungrouped_container_kind(parent),
                        )
                    }
                }
                EventResponse::default()
            }
            LayoutCommand::ToggleFullscreen => {
                // We don't consider this a structural change so don't save the
                // layout.
                let node = self.tree.selection(layout);
                if self.tree.toggle_fullscreen(node) {
                    // If we have multiple windows in the newly fullscreen node,
                    // make sure they are on top.
                    let node_windows = node
                        .traverse_preorder(self.tree.map())
                        .flat_map(|n| self.tree.window_at(n))
                        .collect();
                    EventResponse {
                        frame_overrides: vec![],
                        raise_windows: node_windows,
                        focus_window: None,
                        ..Default::default()
                    }
                } else {
                    EventResponse::default()
                }
            }
            LayoutCommand::Resize { direction, percent } => {
                let percent = percent.clamp(-100.0, 100.0);
                let node = self.tree.selection(layout);
                // A manual resize is a deliberate override, so it releases a
                // size share lock on the resized estate.
                self.tree.clear_estate_size_lock(node);
                self.tree.resize(node, percent / 100.0, direction);
                EventResponse::default()
            }
            LayoutCommand::CycleColumnWidth => {
                if !self.tree.is_scroll_layout(layout) {
                    return EventResponse::default();
                }
                let presets = &self.scroll_config().column_width_presets;
                if presets.is_empty() {
                    return EventResponse::default();
                }
                let selection = self.tree.selection(layout);
                if let Some(col) = self.tree.column_of(layout, selection) {
                    let current_proportion = self.tree.proportion(col).unwrap_or(1.0);
                    let next = presets
                        .iter()
                        .find(|&&p| p > current_proportion + 0.01)
                        .or(presets.first())
                        .copied()
                        .unwrap_or(current_proportion);
                    let delta = next - current_proportion;
                    if delta.abs() > 0.001 {
                        self.tree.resize(col, delta, Direction::Right);
                    }
                }
                EventResponse::default()
            }
            LayoutCommand::ToggleColumnTabbed => {
                if !self.tree.is_scroll_layout(layout) {
                    return EventResponse::default();
                }
                let selection = self.tree.selection(layout);
                if let Some(col) = self.tree.column_of(layout, selection) {
                    let new_kind = match self.tree.container_kind(col) {
                        ContainerKind::Vertical => ContainerKind::Tabbed,
                        ContainerKind::Tabbed => ContainerKind::Vertical,
                        other => other,
                    };
                    self.tree.set_container_kind(col, new_kind);
                }
                EventResponse::default()
            }
            LayoutCommand::ChangeLayoutKind => {
                let old_kind = self.tree.layout_kind(layout);
                let new_kind = match old_kind {
                    LayoutKind::Tree => LayoutKind::Scroll,
                    LayoutKind::Scroll => LayoutKind::Tree,
                };
                let new_layout = Self::convert_layout_kind(
                    &mut self.tree,
                    &self.scroll_cfg,
                    self.focused_window,
                    layout,
                    new_kind,
                );
                if let Some((mapping, _)) = self.active_mapping_mut(space) {
                    mapping.replace_active_layout(new_layout);
                }
                self.viewports.remove(&layout);
                EventResponse::default()
            }
            LayoutCommand::CleanUpSpace => {
                self.tree.reset_weights(layout);
                self.tree.clear_size_locks();
                EventResponse::default()
            }
            LayoutCommand::SetSizeShare(share) => {
                let Some(share) = share.fraction() else {
                    warn!("Ignoring invalid size share {share:?}");
                    return EventResponse::default();
                };
                let Some(wid) = self.focused_window else {
                    return EventResponse::default();
                };
                let Some(node) = self.tree.window_node(layout, wid) else {
                    return EventResponse::default();
                };
                let overflow = self.config.settings.size_share.overflow;
                Self::set_size_share(&mut self.tree, layout, wid, node, share, overflow)
                    .map(|feedback| EventResponse {
                        size_share_feedback: Some(feedback),
                        ..Default::default()
                    })
                    .unwrap_or_default()
            }
            LayoutCommand::ToggleSizeLock => {
                let Some(wid) = self.focused_window else {
                    return EventResponse::default();
                };
                let Some(node) = self.tree.window_node(layout, wid) else {
                    return EventResponse::default();
                };
                Self::toggle_size_lock(&mut self.tree, layout, wid, node)
                    .map(|feedback| EventResponse {
                        size_share_feedback: Some(feedback),
                        ..Default::default()
                    })
                    .unwrap_or_default()
            }
        }
    }

    /// Toggles a size lock at the window's current share.
    fn toggle_size_lock(
        tree: &mut LayoutTree,
        layout: LayoutId,
        wid: WindowId,
        node: NodeId,
    ) -> Option<SizeShareFeedback> {
        // If already locked, release the lock.
        if let Some(current) = tree.estate_size_lock(node) {
            tree.clear_estate_size_lock(node);
            return Some(SizeShareFeedback {
                wid,
                share: current,
                outcome: SizeShareOutcome::Released,
            });
        }

        // Calculate the current share of the estate.
        let share = tree.estate_share(layout, node)?;

        // Don't lock if share would be invalid (>= 1.0 or <= 0.0).
        if share <= 0.0 || share >= 1.0 {
            return None;
        }

        // A fullscreen window can't show its locked share.
        tree.clear_fullscreen_for(node);
        tree.set_size_lock(wid, share);
        Some(SizeShareFeedback {
            wid,
            share,
            outcome: SizeShareOutcome::Applied,
        })
    }

    /// Applies, releases, or refuses a size share for the estate containing
    /// `node`, returning what happened.
    fn set_size_share(
        tree: &mut LayoutTree,
        layout: LayoutId,
        wid: WindowId,
        node: NodeId,
        share: f64,
        overflow: SizeShareOverflow,
    ) -> Option<SizeShareFeedback> {
        let current = tree.estate_size_lock(node);
        if let Some(current) = current
            && (current - share).abs() < 1e-6
        {
            tree.clear_estate_size_lock(node);
            return Some(SizeShareFeedback {
                wid,
                share: current,
                outcome: SizeShareOutcome::Released,
            });
        }

        let locked_elsewhere = tree.locked_share_total(layout) - current.unwrap_or(0.0);
        if locked_elsewhere + share > 1.0 + 1e-9 && overflow == SizeShareOverflow::Reject {
            return Some(SizeShareFeedback {
                wid,
                share,
                outcome: SizeShareOutcome::Rejected,
            });
        }

        // A new share replaces any lock in the same estate, such as a lock on
        // another window in the same group.
        tree.clear_estate_size_lock(node);
        // A fullscreen window can't show its locked share.
        tree.clear_fullscreen_for(node);
        tree.set_size_lock(wid, share);
        Some(SizeShareFeedback {
            wid,
            share,
            outcome: SizeShareOutcome::Applied,
        })
    }

    /// Releases any size share lock on `wid`'s estate, returning whether one
    /// was released.
    ///
    /// Used when the user resizes a locked window by hand. The lock value is
    /// baked into the stored weights so the window maintains its current size.
    pub fn release_size_share(&mut self, wid: WindowId) -> bool {
        let mut released = false;
        for node in self.tree.nodes_for_window(wid) {
            released |= self.tree.clear_estate_size_lock_and_bake(node);
        }
        released
    }
}

impl LayoutManager {
    /// Reorders columns in the layout to match the spatial positions of their windows.
    /// This ensures windows are assigned to columns in left-to-right order based on
    /// their actual screen positions, preventing unnecessary swapping on startup.
    fn reorder_columns_by_position(&mut self, layout: LayoutId) {
        let columns = self.tree.columns(layout);
        if columns.len() <= 1 {
            return;
        }

        // Collect columns with the x position of their first window.
        // Use floating_restore_frames which contains frames for ALL windows,
        // not just the current app's windows.
        let mut columns_with_pos: Vec<(NodeId, f64)> = columns
            .iter()
            .filter_map(|&col| {
                let wid =
                    col.traverse_preorder(self.tree.map()).find_map(|n| self.tree.window_at(n))?;
                let x = self.floating_restore_frames.get(&wid)?.frame.origin.x;
                Some((col, x))
            })
            .collect();

        // Only reorder if we have position info for all columns
        if columns_with_pos.len() != columns.len() {
            return;
        }

        // Sort by x position (left to right)
        columns_with_pos.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        let sorted_columns: Vec<NodeId> = columns_with_pos.iter().map(|(col, _)| *col).collect();

        if sorted_columns == columns {
            return;
        }

        self.tree.reorder_columns(layout, sorted_columns);
    }

    fn is_floating(&self) -> bool {
        if let Some(focus) = self.focused_window {
            self.floating_windows.contains(&focus)
        } else {
            false
        }
    }

    fn add_floating_window(&mut self, wid: WindowId, space: Option<SpaceId>) {
        if let Some(space) = space {
            self.active_floating_windows.insert(space, wid);
        }
        self.floating_windows.insert(wid);
    }

    fn remove_floating_window(&mut self, wid: WindowId, space: Option<SpaceId>) {
        if let Some(space) = space {
            let layout = self.layout(space);
            // Floating removes the window only from the layouts of the context
            // it floated in, so this layout can still have its node.
            let node = self.tree.window_node(layout, wid).unwrap_or_else(|| {
                let selection = self.tree.selection(layout);
                self.tree.add_window_after(layout, selection, wid)
            });
            self.tree.select(node);
            self.active_floating_windows.remove(space, wid);
        }
        self.floating_windows.remove(&wid);
    }

    /// Removes the window from every layout of the context the Space shows,
    /// on every Space and screen size. Without a Space, removes it from every
    /// layout.
    fn remove_window_from_shown_context(&mut self, wid: WindowId, space: Option<SpaceId>) {
        let Some(space) = space else {
            self.tree.remove_window(wid);
            return;
        };
        let layouts: Vec<LayoutId> = match self.shown_context(space) {
            ContextKey::Everything => {
                self.layout_mapping.values().flat_map(|mapping| mapping.layouts()).collect()
            }
            key => self
                .context_layouts
                .iter()
                .filter(|((_, other), _)| *other == key)
                .flat_map(|(_, mapping)| mapping.layouts())
                .collect(),
        };
        for layout in layouts {
            self.tree.remove_window_from(layout, wid);
        }
    }

    /// Removes a window that left the Space from the Space's layouts. While
    /// contexts are in use, those are the layouts of every context on the
    /// Space, for every screen size. Otherwise only the layout the Space
    /// shows loses it.
    fn remove_window_from_space(&mut self, space: SpaceId, wid: WindowId, contexts_in_use: bool) {
        if !contexts_in_use {
            self.tree.remove_window_from(self.layout(space), wid);
            return;
        }
        for layout in self.space_layouts(space) {
            self.tree.remove_window_from(layout, wid);
        }
    }

    /// Every layout of the Space: the layout it shows and the layout of every
    /// context, for every screen size.
    fn space_layouts(&self, space: SpaceId) -> Vec<LayoutId> {
        self.layout_mapping
            .get(&space)
            .into_iter()
            .chain(
                self.context_layouts
                    .iter()
                    .filter(|((other, _), _)| *other == space)
                    .map(|(_, mapping)| mapping),
            )
            .flat_map(|mapping| mapping.layouts())
            .collect()
    }

    /// Whether the window floats, which keeps it out of the tile layouts.
    pub fn is_floating_window(&self, wid: WindowId) -> bool {
        self.floating_windows.contains(&wid)
    }

    /// Moves the window from the Space `from` to the Space `to`, which a
    /// `per_screen` switch gives the screen it changed (R8, R9). The window
    /// keeps its place in every layout of the Space it leaves, so it returns
    /// there, while those layouts close the gap. A floating window moves its
    /// floating bookkeeping instead; the reactor places it.
    pub fn move_window_to_space(&mut self, wid: WindowId, from: SpaceId, to: SpaceId) {
        if from == to {
            return;
        }
        if self.floating_windows.contains(&wid) {
            self.active_floating_windows.remove(from, wid);
            self.add_floating_window(wid, Some(to));
            return;
        }
        for layout in self.space_layouts(from) {
            self.tree.stash_window(layout, wid);
        }
        let layout = self.layout(to);
        if self.tree.window_node(layout, wid).is_none() {
            let selection = self.tree.selection(layout);
            self.tree.add_window_after(layout, selection, wid);
        }
    }
}

/// Tracks which floating windows are present on each space.
#[derive(Default)]
struct ActiveFloatingWindows {
    by_space: HashMap<SpaceId, HashMap<pid_t, HashSet<WindowId>>>,
}

impl ActiveFloatingWindows {
    fn select_tab(
        &mut self,
        previous: WindowId,
        selected: WindowId,
        members: &[WindowId],
        floating: bool,
    ) {
        for by_pid in self.by_space.values_mut() {
            if let Some(windows) = by_pid.get_mut(&previous.pid) {
                let was_active = windows.contains(&previous);
                windows.retain(|wid| *wid != previous && !members.contains(wid));
                if was_active && floating {
                    windows.insert(selected);
                }
            }
        }
    }

    fn insert(&mut self, space: SpaceId, wid: WindowId) {
        self.by_space.entry(space).or_default().entry(wid.pid).or_default().insert(wid);
    }

    fn remove(&mut self, space: SpaceId, wid: WindowId) {
        if let Some(by_pid) = self.by_space.get_mut(&space)
            && let Some(wids) = by_pid.get_mut(&wid.pid)
        {
            wids.remove(&wid);
        }
    }

    /// Clears the set of floating windows for one app on one space and returns a
    /// handle for inserting that app's current floating windows.
    fn reset_app(&mut self, space: SpaceId, pid: pid_t) -> &mut HashSet<WindowId> {
        let set = self.by_space.entry(space).or_default().entry(pid).or_default();
        set.clear();
        set
    }

    fn in_space(&self, space: SpaceId) -> impl Iterator<Item = WindowId> + '_ {
        self.by_space
            .get(&space)
            .into_iter()
            .flat_map(|by_pid| by_pid.values().flatten().copied())
    }
}

impl LayoutManager {
    pub fn calculate_layout(
        &self,
        space: SpaceId,
        screen: CGRect,
        config: &Config,
    ) -> Vec<(WindowId, CGRect)> {
        let layout = self.layout(space);
        //debug!("{}", self.tree.draw_tree(space));
        let frames = self.tree.calculate_layout(layout, screen, config);
        if self.scroll_enabled && self.tree.is_scroll_layout(layout) {
            if let Some(vp) = self.viewports.get(&layout) {
                return vp.apply_viewport_to_frames(screen, frames, Instant::now());
            }
        }
        frames
    }

    pub fn calculate_layout_and_groups(
        &self,
        space: SpaceId,
        screen: CGRect,
        config: &Config,
    ) -> (Vec<(WindowId, CGRect)>, Vec<crate::model::GroupBarInfo>) {
        let layout = self.layout(space);
        let (sizes, mut groups) = self.tree.calculate_layout_and_groups(layout, screen, config);
        if self.is_floating() {
            // Make sure group bars don't cover the floating windows.
            for group in &mut groups {
                group.is_on_top = false;
            }
        }
        if self.scroll_enabled && self.tree.is_scroll_layout(layout) {
            if let Some(vp) = self.viewports.get(&layout) {
                let transformed = vp.apply_viewport_to_frames(screen, sizes, Instant::now());
                for group in &mut groups {
                    group.indicator_frame = vp.offset_rect(group.indicator_frame, Instant::now());
                }
                return (transformed, groups);
            }
        }
        (sizes, groups)
    }

    /// The smallest size observed for the window, if any constraint is known.
    pub fn window_min_size(&self, wid: WindowId) -> Option<CGSize> {
        self.tree.window_min_size(wid)
    }

    /// Records a lower bound on the window's size, merging per axis.
    pub fn note_window_min_size(&mut self, wid: WindowId, min_size: CGSize) {
        self.tree.note_window_min_size(wid, min_size);
    }

    /// Lowers the recorded minimum for the window to at most `size` per axis.
    pub fn relax_window_min_size(&mut self, wid: WindowId, size: CGSize) {
        self.tree.relax_window_min_size(wid, size);
    }

    /// Whether the space's active layout places windows by scrolling, in which
    /// case some windows are intentionally off screen.
    pub fn is_scroll_space(&self, space: SpaceId) -> bool {
        self.scroll_enabled
            && self.try_layout(space).is_some_and(|layout| self.tree.is_scroll_layout(layout))
    }
}

impl LayoutManager {
    fn scroll_config(&self) -> &ScrollConfig {
        &self.scroll_cfg
    }

    fn add_scroll_window(&mut self, layout: LayoutId, wid: WindowId) {
        let new_column = self.scroll_config().new_window_in_column == NewWindowPlacement::NewColumn;
        self.tree.add_window_to_scroll_column_with_visible(
            layout,
            wid,
            new_column,
            self.scroll_config().visible_columns,
        );
    }

    pub fn viewport(&self, layout: LayoutId) -> Option<&ViewportState> {
        self.viewports.get(&layout)
    }

    pub fn viewport_mut(&mut self, layout: LayoutId, screen_width: f64) -> &mut ViewportState {
        self.viewports.entry(layout).or_insert_with(|| ViewportState::new(screen_width))
    }

    pub fn clear_user_scrolling(&mut self, space: SpaceId) {
        let layout = self.layout(space);
        if let Some(vp) = self.viewports.get_mut(&layout) {
            vp.user_scrolling = false;
        }
    }

    pub fn update_viewport_for_focus(&mut self, space: SpaceId, screen: CGRect, config: &Config) {
        if !self.scroll_enabled {
            return;
        }
        let layout = self.layout(space);
        if !self.tree.is_scroll_layout(layout) {
            return;
        }

        if self.viewport(layout).map_or(false, |vp| vp.user_scrolling) {
            return;
        }

        let frames = self.tree.calculate_layout(layout, screen, config);
        let selection = self.tree.selection(layout);
        let sel_wid = self.tree.window_at(selection);
        let columns = self.tree.columns(layout);
        let col = self.tree.column_of(layout, selection);
        let center_mode = config.settings.experimental.scroll.center_focused_column;
        let gap = config.settings.inner_gap;

        let vp = self.viewport_mut(layout, screen.size.width);
        vp.set_screen_width(screen.size.width);

        if let Some(wid) = sel_wid {
            if let Some((_, frame)) = frames.iter().find(|(w, _)| *w == wid) {
                if let Some(c) = col {
                    let col_idx = columns.iter().position(|&n| n == c).unwrap_or(0);
                    vp.ensure_column_visible(
                        col_idx,
                        frame.origin.x,
                        frame.size.width,
                        center_mode,
                        gap,
                        Instant::now(),
                    );
                }
            }
        }
    }

    pub fn has_active_scroll_animation(&self) -> bool {
        if !self.scroll_enabled {
            return false;
        }
        self.viewports.values().any(|vp| vp.is_animating(Instant::now()))
    }

    pub fn tick_viewports(&mut self) {
        for vp in self.viewports.values_mut() {
            vp.tick(Instant::now());
        }
    }

    pub fn handle_scroll_wheel(
        &mut self,
        space: SpaceId,
        delta_x: f64,
        screen: &CGRect,
        config: &crate::config::ScrollConfig,
    ) -> EventResponse {
        if !self.scroll_enabled {
            return EventResponse::default();
        }
        let layout = self.layout(space);
        if !self.tree.is_scroll_layout(layout) {
            return EventResponse::default();
        }

        let columns = self.tree.columns(layout);
        let col_count = columns.len();
        if col_count == 0 {
            return EventResponse::default();
        }

        let step_threshold = screen.size.width / col_count.min(3) as f64;

        let delta = if config.invert_scroll_direction {
            -delta_x
        } else {
            delta_x
        };
        let scaled_delta = delta * config.scroll_sensitivity;

        let is_discrete = delta_x.abs() < 10.0 && delta_x.fract() == 0.0;
        let (effective_delta, effective_threshold) = if is_discrete {
            (scaled_delta.signum() * step_threshold, step_threshold)
        } else {
            (scaled_delta, step_threshold)
        };

        let vp = self.viewport_mut(layout, screen.size.width);
        vp.set_screen_width(screen.size.width);

        let steps = match vp.accumulate_scroll(effective_delta, effective_threshold) {
            Some(s) => s,
            None => return EventResponse::default(),
        };

        let selection = self.tree.selection(layout);
        let direction = if steps < 0 {
            Direction::Right
        } else {
            Direction::Left
        };
        let abs_steps = steps.unsigned_abs().min(16) as usize;

        let mut current = selection;
        for _ in 0..abs_steps {
            let next = if self.scroll_config().infinite_loop {
                self.tree.traverse_scroll_wrapping(layout, current, direction)
            } else {
                self.tree.traverse(current, direction)
            };
            match next {
                Some(n) => current = n,
                None => break,
            }
        }

        if current == selection {
            return EventResponse::default();
        }

        self.clear_user_scrolling(space);
        let focus_window = self.tree.window_at(current);
        let raise_windows = self.tree.select_returning_surfaced_windows(current);
        EventResponse {
            frame_overrides: vec![],
            focus_window,
            raise_windows,
            ..Default::default()
        }
    }

    pub(crate) fn hit_test_scroll_edges(
        &self,
        space: SpaceId,
        point: CGPoint,
        screen: CGRect,
        config: &Config,
    ) -> Option<(NodeId, NodeId, ResizeEdge)> {
        if !self.scroll_enabled {
            return None;
        }
        let layout = self.try_layout(space)?;
        if !self.tree.is_scroll_layout(layout) {
            return None;
        }
        let frames = self.calculate_layout(space, screen, config);
        for (wid, frame) in &frames {
            let edges = detect_edges(point, *frame);
            if !edges.is_empty() {
                let window_node = self.tree.window_node(layout, *wid)?;
                let column_node = self.tree.column_of(layout, window_node)?;
                return Some((column_node, window_node, edges));
            }
        }
        None
    }

    pub fn hit_test_scroll_window(
        &self,
        space: SpaceId,
        point: CGPoint,
        screen: CGRect,
        config: &Config,
    ) -> Option<(WindowId, NodeId)> {
        if !self.scroll_enabled {
            return None;
        }
        let layout = self.try_layout(space)?;
        if !self.tree.is_scroll_layout(layout) {
            return None;
        }
        let frames = self.calculate_layout(space, screen, config);
        for (wid, frame) in &frames {
            if frame.contains(point) {
                let node = self.tree.window_node(layout, *wid)?;
                return Some((*wid, node));
            }
        }
        None
    }

    pub(crate) fn begin_interactive_resize(
        &mut self,
        column: NodeId,
        window: NodeId,
        edges: ResizeEdge,
        mouse: CGPoint,
    ) -> bool {
        if self.interactive_resize.is_some() {
            return false;
        }
        self.interactive_resize = Some(InteractiveScrollResize {
            column_node: column,
            window_node: window,
            edges,
            last_mouse: mouse,
        });
        true
    }

    pub fn update_interactive_resize(&mut self, mouse: CGPoint, screen: CGRect) -> bool {
        let Some(state) = self.interactive_resize.as_mut() else {
            return false;
        };
        let dx = mouse.x - state.last_mouse.x;
        let dy = mouse.y - state.last_mouse.y;
        state.last_mouse = mouse;

        let mut changed = false;
        if state.edges.has_horizontal() {
            let ratio = dx / screen.size.width;
            let direction = if state.edges.0 & ResizeEdge::LEFT != 0 {
                Direction::Left
            } else {
                Direction::Right
            };
            let col = state.column_node;
            if self.tree.resize(col, ratio, direction) {
                changed = true;
            }
        }
        if state.edges.has_vertical() {
            let ratio = dy / screen.size.height;
            let direction = if state.edges.0 & ResizeEdge::TOP != 0 {
                Direction::Up
            } else {
                Direction::Down
            };
            let win = state.window_node;
            if self.tree.resize(win, ratio, direction) {
                changed = true;
            }
        }
        changed
    }

    pub fn end_interactive_resize(&mut self, space: SpaceId, screen: CGRect, config: &Config) {
        if self.interactive_resize.take().is_some() {
            self.clear_user_scrolling(space);
            self.update_viewport_for_focus(space, screen, config);
        }
    }

    pub fn begin_interactive_move(
        &mut self,
        space: SpaceId,
        wid: WindowId,
        node: NodeId,
        mouse: CGPoint,
    ) -> bool {
        if self.interactive_resize.is_some() || self.interactive_move.is_some() {
            return false;
        }
        let layout_id = self.layout(space);
        self.interactive_move = Some(InteractiveScrollMove {
            layout_id,
            window_id: wid,
            window_node: node,
            start_mouse: mouse,
            drag_active: false,
        });
        true
    }

    pub fn update_interactive_move(
        &mut self,
        mouse: CGPoint,
        screen: CGRect,
        config: &Config,
    ) -> bool {
        let Some(state) = self.interactive_move.as_mut() else {
            return false;
        };
        if !state.drag_active {
            let dx = mouse.x - state.start_mouse.x;
            let dy = mouse.y - state.start_mouse.y;
            if (dx * dx + dy * dy).sqrt() < MOVE_DRAG_THRESHOLD {
                return false;
            }
            state.drag_active = true;
        }
        let source_node = state.window_node;
        let source_wid = state.window_id;
        let layout = state.layout_id;
        let frames = self.tree.calculate_layout(layout, screen, config);
        let vp_opt = self.viewports.get(&layout);

        for (wid, frame) in &frames {
            if *wid == source_wid {
                continue;
            }

            let target_frame;
            if let Some(vp) = vp_opt {
                if !vp.is_visible(*frame, Instant::now()) {
                    continue;
                }
                target_frame = vp.offset_rect(*frame, Instant::now());
            } else {
                target_frame = *frame;
            }

            if target_frame.contains(mouse)
                && let Some(target_node) = self.tree.window_node(layout, *wid)
            {
                self.tree.swap_windows(source_node, target_node);
                if let Some(state) = self.interactive_move.as_mut() {
                    state.window_node = target_node;
                }
                return true;
            }
        }
        false
    }

    pub fn end_interactive_move(&mut self, space: SpaceId, screen: CGRect, config: &Config) {
        if self.interactive_move.take().is_some() {
            self.clear_user_scrolling(space);
            self.update_viewport_for_focus(space, screen, config);
        }
    }

    pub fn cancel_interactive_state(&mut self) {
        self.interactive_resize = None;
        self.interactive_move = None;
        self.interactive_drag = None;
    }

    pub fn has_interactive_state(&self) -> bool {
        self.interactive_resize.is_some()
            || self.interactive_move.is_some()
            || self.interactive_drag.is_some()
    }

    /// Hit test to find which tiled window is at a given point.
    pub fn hit_test_window(
        &self,
        space: SpaceId,
        point: CGPoint,
        screen: CGRect,
        config: &Config,
    ) -> Option<(WindowId, NodeId)> {
        let layout = self.try_layout(space)?;
        let frames = self.tree.calculate_layout(layout, screen, config);
        for (wid, frame) in &frames {
            if frame.contains(point) {
                let node = self.tree.window_node(layout, *wid)?;
                return Some((*wid, node));
            }
        }
        None
    }

    /// Begin a drag-to-rearrange operation.
    ///
    /// Always returns false since dragging from window content is no longer
    /// supported. Only title bar drags work, which are handled by the reactor's
    /// TitleBarDrag tracking.
    #[allow(unused_variables)]
    pub fn begin_interactive_drag(
        &mut self,
        space: SpaceId,
        wid: WindowId,
        node: NodeId,
        mouse: CGPoint,
        screen: CGRect,
        config: &Config,
    ) -> bool {
        false
    }

    /// Update drag state based on current mouse position.
    ///
    /// Returns a `DragUpdate` indicating what changed, for use by the reactor
    /// to animate preview positions or restore original positions.
    pub fn update_interactive_drag(
        &mut self,
        mouse: CGPoint,
        screen: CGRect,
        config: &Config,
    ) -> DragUpdate {
        let Some(state) = self.interactive_drag.as_mut() else {
            return DragUpdate::NoChange;
        };
        let drag_cfg = &config.settings.drag_drop;

        // Check drag threshold
        if !state.drag_active {
            let dx = mouse.x - state.start_mouse.x;
            let dy = mouse.y - state.start_mouse.y;
            if (dx * dx + dy * dy).sqrt() < drag_cfg.drag_threshold {
                return DragUpdate::NoChange;
            }
            state.drag_active = true;
        }

        let layout = state.layout_id;
        let source_wid = state.source_wid;
        let source_node = state.source_node;
        let frames = self.tree.calculate_layout(layout, screen, config);

        // Find target window under cursor
        let mut new_target: Option<HoverTarget> = None;
        for (wid, frame) in &frames {
            if *wid == source_wid {
                continue;
            }
            if frame.contains(mouse) {
                if let Some(node) = self.tree.window_node(layout, *wid) {
                    let zone = DropZoneRegion::from_point(mouse, *frame, drag_cfg.edge_zone_ratio);
                    new_target = Some(HoverTarget {
                        node,
                        wid: *wid,
                        frame: *frame,
                        zone,
                    });
                    break;
                }
            }
        }

        // Update hover target
        state.hover_target = new_target.clone();

        // Compute action based on zone
        let action = new_target.as_ref().and_then(|target| {
            let parent = target.node.parent(self.tree.map())?;
            let parent_orientation = self.tree.container_kind(parent).orientation();
            Some(target.zone.to_action(target.node, parent_orientation))
        });

        state.current_action = action;

        // Check if action changed from preview state
        let last_action = state.preview.last_action;
        let action_changed = action != last_action;

        if !action_changed {
            return DragUpdate::NoChange;
        }

        // Update preview state and extract data we need before releasing the borrow
        state.preview.last_action = action;
        let target_wid = state.hover_target.as_ref().map(|t| t.wid);
        let original_frames: Vec<_> = state
            .preview
            .original_frames
            .iter()
            .map(|(wid, frame)| (*wid, *frame))
            .collect();

        match (action, target_wid) {
            (Some(action), Some(target_wid)) => {
                // Calculate preview frames for the new action
                let preview_frames = self.calculate_preview_frames(
                    layout, source_wid, target_wid, action, screen, config,
                );
                DragUpdate::PreviewChanged {
                    source_node,
                    action,
                    preview_frames,
                }
            }
            (None, _) if last_action.is_some() => {
                // Action became None, restore original positions
                DragUpdate::RestoreOriginal { original_frames }
            }
            _ => DragUpdate::NoChange,
        }
    }

    /// End the drag operation and return the action to apply.
    pub fn end_interactive_drag(&mut self, _space: SpaceId) -> Option<(NodeId, DropAction)> {
        let state = self.interactive_drag.take()?;
        if !state.drag_active {
            return None;
        }
        state.current_action.map(|a| (state.source_node, a))
    }

    /// Calculate preview frames by simulating a drop action on a temporary layout clone.
    ///
    /// Returns frames for all windows except the dragged window.
    fn calculate_preview_frames(
        &mut self,
        layout: LayoutId,
        source_wid: WindowId,
        target_wid: WindowId,
        action: DropAction,
        screen: CGRect,
        config: &Config,
    ) -> Vec<(WindowId, CGRect)> {
        // Clone the layout to avoid modifying the real tree.
        let temp_layout = self.tree.clone_layout(layout);

        // Find the corresponding nodes in the cloned layout.
        let Some(source_node) = self.tree.window_node(temp_layout, source_wid) else {
            self.tree.remove_layout(temp_layout);
            return Vec::new();
        };
        let Some(target_node) = self.tree.window_node(temp_layout, target_wid) else {
            self.tree.remove_layout(temp_layout);
            return Vec::new();
        };

        // Apply the action to the cloned layout.
        match action {
            DropAction::Swap { .. } => {
                self.tree.swap_windows(source_node, target_node);
            }
            DropAction::Insert { before, .. } => {
                if before {
                    self.tree.move_node_before(target_node, source_node);
                } else {
                    self.tree.move_node_after(target_node, source_node);
                }
            }
            DropAction::Split { orientation, .. } => {
                self.tree.nest_in_container(temp_layout, target_node, orientation);
                self.tree.move_node_after(target_node, source_node);
            }
        }

        // Calculate frames from the modified clone.
        let frames = self.tree.calculate_layout(temp_layout, screen, config);

        // Clean up the temporary layout.
        self.tree.remove_layout(temp_layout);

        // Return all frames except the dragged window.
        frames.into_iter().filter(|(wid, _)| *wid != source_wid).collect()
    }

    /// Compute drop zone information for a title bar drag.
    /// Returns the target node, window, zone, and action if a valid target is found.
    pub fn compute_titlebar_drag_action(
        &self,
        space: SpaceId,
        source_wid: WindowId,
        position: CGPoint,
        screen: CGRect,
        config: &Config,
    ) -> Option<(NodeId, WindowId, DropZoneRegion, DropAction)> {
        let layout = self.try_layout(space)?;
        let frames = self.tree.calculate_layout(layout, screen, config);
        let edge_ratio = config.settings.drag_drop.edge_zone_ratio;

        // Find window under the drop position (excluding source)
        for (wid, frame) in &frames {
            if *wid == source_wid {
                continue;
            }
            if frame.contains(position) {
                if let Some(target_node) = self.tree.window_node(layout, *wid) {
                    let zone = DropZoneRegion::from_point(position, *frame, edge_ratio);
                    let parent = target_node.parent(self.tree.map())?;
                    let parent_orientation = self.tree.container_kind(parent).orientation();
                    let action = zone.to_action(target_node, parent_orientation);
                    return Some((target_node, *wid, zone, action));
                }
            }
        }
        None
    }

    /// Compute a drop action based on a window's current position.
    /// Used for title bar drags when window_drag is disabled.
    pub fn compute_drop_action_for_position(
        &self,
        space: SpaceId,
        source_wid: WindowId,
        position: CGPoint,
        screen: CGRect,
        config: &Config,
    ) -> Option<DropAction> {
        self.compute_titlebar_drag_action(space, source_wid, position, screen, config)
            .map(|(_, _, _, action)| action)
    }

    /// Compute a drop action and preview frames for title bar drags.
    /// Returns target info (node, wid, zone, action) and preview frames if a valid drop target
    /// is found.
    pub fn compute_titlebar_preview(
        &mut self,
        space: SpaceId,
        source_wid: WindowId,
        position: CGPoint,
        screen: CGRect,
        config: &Config,
    ) -> Option<(DropAction, Vec<(WindowId, CGRect)>)> {
        let layout = self.try_layout(space)?;
        let (_, target_wid, _, action) =
            self.compute_titlebar_drag_action(space, source_wid, position, screen, config)?;
        let preview_frames =
            self.calculate_preview_frames(layout, source_wid, target_wid, action, screen, config);
        Some((action, preview_frames))
    }

    /// Apply a drop action to the layout tree.
    pub fn apply_drop_action(&mut self, space: SpaceId, source_node: NodeId, action: DropAction) {
        // Validate that source node still exists (window may have closed during drag).
        if !self.tree.node_exists(source_node) {
            return;
        }
        let target_node = match &action {
            DropAction::Swap { target_node }
            | DropAction::Insert { target_node, .. }
            | DropAction::Split { target_node, .. } => *target_node,
        };
        if !self.tree.node_exists(target_node) {
            return;
        }

        let layout = self.layout(space);

        // Clear min_sizes for rearranged windows so they can adapt to their
        // new layout context without being constrained by stale size limits.
        if let Some(source_wid) = self.tree.window_at(source_node) {
            self.tree.clear_window_min_size(source_wid);
        }

        match action {
            DropAction::Swap { target_node } => {
                if let Some(target_wid) = self.tree.window_at(target_node) {
                    self.tree.clear_window_min_size(target_wid);
                }
                self.tree.swap_windows(source_node, target_node);
            }
            DropAction::Insert { target_node, before } => {
                if before {
                    self.tree.move_node_before(target_node, source_node);
                } else {
                    self.tree.move_node_after(target_node, source_node);
                }
            }
            DropAction::Split { target_node, orientation } => {
                // Clear the target window's min_size too since both windows
                // are now in a new container and should adapt.
                if let Some(target_wid) = self.tree.window_at(target_node) {
                    self.tree.clear_window_min_size(target_wid);
                }
                // Create container around target, then insert source
                self.tree.nest_in_container(layout, target_node, orientation);
                self.tree.move_node_after(target_node, source_node);
            }
        }
    }

    /// Generate debug drop zones for all windows on a space.
    ///
    /// Returns drop zones showing the 5 regions (center + 4 edges) for each
    /// tiled window, useful for visualizing drop zone boundaries.
    pub fn generate_debug_drop_zones(
        &self,
        space: SpaceId,
        screen: CGRect,
        config: &Config,
        screen_index: i32,
    ) -> Vec<crate::ui::swift_bridge::DropZone> {
        use crate::ui::swift_bridge::{DropActionType, DropZone};

        let Some(layout) = self.try_layout(space) else {
            return vec![];
        };

        let frames = self.tree.calculate_layout(layout, screen, config);
        let edge_ratio = config.settings.drag_drop.edge_zone_ratio;
        let mut zones = Vec::new();

        for (wid, frame) in &frames {
            let Some(node) = self.tree.window_node(layout, *wid) else {
                continue;
            };

            // Get parent orientation to determine action types
            let parent_orientation = node
                .parent(self.tree.map())
                .and_then(|p| Some(self.tree.container_kind(p).orientation()))
                .unwrap_or(Orientation::Horizontal);

            // Zones are in screen-local coordinates (0,0 to width,height)
            let x = frame.origin.x as f32;
            let y = frame.origin.y as f32;
            let w = frame.size.width as f32;
            let h = frame.size.height as f32;
            // Left/right zones use edge_ratio (15%), bottom zone uses 1/3
            let edge_w = w * edge_ratio as f32;
            let bottom_h = h / 3.0;
            // Use wsid if available, otherwise use a hash of pid
            let wid_u64 = wid.wsid().map(|w| w.0 as u64).unwrap_or(wid.pid as u64);

            // Center zone (swap) - extends to top since top zone is disabled
            zones.push(DropZone::with_action(
                x + edge_w,
                y,
                w - 2.0 * edge_w,
                h - bottom_h,
                4, // tab position type (center-ish)
                wid_u64,
                DropActionType::Swap,
                false,
                0.0,
                screen_index,
            ));

            // Left edge zone - extends to top since top zone is disabled
            let left_action = if parent_orientation == Orientation::Horizontal {
                DropActionType::Insert
            } else {
                DropActionType::Split
            };
            zones.push(DropZone::with_action(
                x,
                y,
                edge_w,
                h - bottom_h,
                0, // left
                wid_u64,
                left_action,
                false,
                0.0,
                screen_index,
            ));

            // Right edge zone - extends to top since top zone is disabled
            let right_action = left_action;
            zones.push(DropZone::with_action(
                x + w - edge_w,
                y,
                edge_w,
                h - bottom_h,
                1, // right
                wid_u64,
                right_action,
                false,
                0.0,
                screen_index,
            ));

            // Bottom edge zone (1/3 of height for horizontal split)
            let bottom_action = if parent_orientation == Orientation::Vertical {
                DropActionType::Insert
            } else {
                DropActionType::Split
            };
            zones.push(DropZone::with_action(
                x + edge_w,
                y + h - bottom_h,
                w - 2.0 * edge_w,
                bottom_h,
                3, // bottom
                wid_u64,
                bottom_action,
                false,
                0.0,
                screen_index,
            ));
        }

        zones
    }
}

impl LayoutManager {
    /// The active context of the Space, as the reactor last passed it.
    fn active_context(&self, space: SpaceId) -> ContextKey {
        self.active_contexts.get(&space).copied().unwrap_or(ContextKey::Everything)
    }

    /// The context whose layout the Space shows: its active context, or
    /// Everything when the active context has no mapping on the Space.
    fn shown_context(&self, space: SpaceId) -> ContextKey {
        let key = self.active_context(space);
        if key != ContextKey::Everything && !self.context_layouts.contains_key(&(space, key)) {
            error!(
                ?space,
                ?key,
                "The active context has no layout on this space; using Everything's"
            );
            return ContextKey::Everything;
        }
        key
    }

    /// The context's mapping on the Space.
    fn mapping(&self, space: SpaceId, key: ContextKey) -> Option<&SpaceLayoutMapping> {
        match key {
            ContextKey::Everything => self.layout_mapping.get(&space),
            key => self.context_layouts.get(&(space, key)),
        }
    }

    /// The context's mapping on the Space, with the tree that holds its
    /// layouts.
    fn mapping_mut(
        &mut self,
        space: SpaceId,
        key: ContextKey,
    ) -> Option<(&mut SpaceLayoutMapping, &mut LayoutTree)> {
        let mapping = match key {
            ContextKey::Everything => self.layout_mapping.get_mut(&space),
            key => self.context_layouts.get_mut(&(space, key)),
        }?;
        Some((mapping, &mut self.tree))
    }

    /// The mapping whose active layout the Space shows.
    fn active_mapping(&self, space: SpaceId) -> Option<&SpaceLayoutMapping> {
        self.mapping(space, self.shown_context(space))
    }

    /// The mapping whose active layout the Space shows, with the tree that
    /// holds its layouts.
    fn active_mapping_mut(
        &mut self,
        space: SpaceId,
    ) -> Option<(&mut SpaceLayoutMapping, &mut LayoutTree)> {
        self.mapping_mut(space, self.shown_context(space))
    }

    /// Gives the context its first layout on the Space: a copy of the layout
    /// the Space shows, without the windows that aren't members.
    fn create_context_mapping(&mut self, space: SpaceId, size: CGSize, context: &ActiveContext) {
        let Some(shown) = self.try_layout(space) else {
            return;
        };
        let layout = self.tree.clone_layout(shown);
        let non_members: Vec<WindowId> = self
            .tree
            .root(layout)
            .traverse_postorder(self.tree.map())
            .filter_map(|node| self.tree.window_at(node))
            .filter(|wid| !context.members.contains(wid))
            .collect();
        for wid in non_members {
            self.tree.remove_window_from(layout, wid);
        }
        self.context_layouts.insert(
            (space, context.key),
            SpaceLayoutMapping::from_layout(size, layout),
        );
    }

    /// The named contexts that have a layout on some Space.
    pub fn context_ids(&self) -> impl Iterator<Item = ContextId> + '_ {
        self.context_layouts.keys().filter_map(|&(_, key)| match key {
            ContextKey::Named(id) => Some(id),
            ContextKey::Everything | ContextKey::Unsorted => None,
        })
    }

    /// Whether the layout leaves the window alone, as it does with panels and
    /// other windows that aren't on the normal layer.
    pub fn is_untracked(&self, info: &LayoutWindowInfo) -> bool {
        classify_window(&self.window_rules, info) == WindowClass::Untracked
    }

    /// Deletes every layout of the context, on every Space and screen size.
    pub fn remove_context_layouts(&mut self, id: ContextId) {
        self.retain_context_layouts(|other| other != id);
    }

    /// Deletes every layout of each named context for which `keep` returns
    /// false. Unsorted's layouts stay. A Space whose active context is deleted
    /// shows Everything until the next `SpaceExposed`. A mouse move, resize,
    /// or drag on a deleted layout ends.
    pub fn retain_context_layouts(&mut self, mut keep: impl FnMut(ContextId) -> bool) {
        let mut deleted = |key: ContextKey| matches!(key, ContextKey::Named(id) if !keep(id));
        self.active_contexts.retain(|_, key| !deleted(*key));
        let removed: Vec<_> =
            self.context_layouts.keys().copied().filter(|&(_, key)| deleted(key)).collect();
        let mut layouts = Vec::new();
        for key in removed {
            let Some(mapping) = self.context_layouts.remove(&key) else {
                continue;
            };
            for layout in mapping.layouts() {
                self.tree.remove_layout(layout);
                self.viewports.remove(&layout);
                layouts.push(layout);
            }
        }
        self.interactive_move.take_if(|state| layouts.contains(&state.layout_id));
        self.interactive_drag.take_if(|state| layouts.contains(&state.layout_id));
        // A resize holds nodes, which are gone once their layout is removed.
        self.interactive_resize.take_if(|state| {
            !self.tree.node_exists(state.column_node) || !self.tree.node_exists(state.window_node)
        });
    }

    fn try_layout(&self, space: SpaceId) -> Option<LayoutId> {
        self.active_mapping(space)?.active_layout().into()
    }

    fn layout(&self, space: SpaceId) -> LayoutId {
        self.try_layout(space).unwrap()
    }

    fn top_layer_windows(&self, space: SpaceId) -> Vec<WindowId> {
        if self.is_floating() {
            // For now don't mess with window order if a floating window is focused.
            return Vec::new();
        }
        let Some(layout) = self.try_layout(space) else {
            return Vec::new();
        };
        self.tree.visible_windows_under(self.tree.root(layout))
    }

    pub fn load(path: PathBuf, config: Arc<Config>) -> anyhow::Result<Self> {
        let mut buf = String::new();
        File::open(path)?.read_to_string(&mut buf)?;
        let mut manager: Self = ron::from_str(&buf)?;
        // Only the field, not `set_config`: the caller applies the config, and
        // `set_config` also converts layouts the config no longer allows.
        manager.config = config;
        Ok(manager)
    }

    pub fn save(&self, path: PathBuf) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        File::create(path)?.write_all(self.serialize_to_string().as_bytes())?;
        Ok(())
    }

    pub fn serialize_to_string(&self) -> String {
        ron::ser::to_string(&self).unwrap()
    }

    // This seems a bit messy, but it's simpler and more robust to write some
    // reactor tests as integration tests with this actor.
    #[cfg(test)]
    pub(super) fn selected_window(&mut self, space: SpaceId) -> Option<WindowId> {
        let layout = self.layout(space);
        self.tree.window_at(self.tree.selection(layout))
    }

    #[cfg(test)]
    pub(super) fn active_layout_kind(&self, space: SpaceId) -> LayoutKind {
        self.tree.layout_kind(self.layout(space))
    }

    pub(crate) fn floating_windows_in_space(&self, space: SpaceId) -> BTreeSet<WindowId> {
        self.active_floating_windows.in_space(space).collect()
    }

    #[cfg(test)]
    pub(super) fn floating_restore_frame(&self, wid: WindowId) -> Option<CGRect> {
        self.floating_restore_frames.get(&wid).map(|restore| restore.frame)
    }

    /// Every window present in any layout. Used by the restore snapshot tests to
    /// confirm a deserialized layout retained its window mapping.
    #[cfg(test)]
    pub(crate) fn all_windows(&self) -> std::collections::BTreeSet<WindowId> {
        self.tree
            .layouts()
            .flat_map(|layout| self.tree.visible_windows_under(self.tree.root(layout)))
            .collect()
    }
}

// TODO: detect_edges does not account for screen boundaries.
// A window flush against a screen edge should not offer resize on that edge.
fn detect_edges(point: CGPoint, frame: CGRect) -> ResizeEdge {
    let threshold = RESIZE_EDGE_THRESHOLD;
    let expanded = CGRect::new(
        CGPoint::new(frame.origin.x - threshold, frame.origin.y - threshold),
        CGSize::new(
            frame.size.width + threshold * 2.0,
            frame.size.height + threshold * 2.0,
        ),
    );
    if !expanded.contains(point) {
        return ResizeEdge(0);
    }
    let inner = CGRect::new(
        CGPoint::new(frame.origin.x + threshold, frame.origin.y + threshold),
        CGSize::new(
            (frame.size.width - threshold * 2.0).max(0.0),
            (frame.size.height - threshold * 2.0).max(0.0),
        ),
    );
    if inner.contains(point) {
        return ResizeEdge(0);
    }
    let mut edges = 0u8;
    if point.x < frame.origin.x + threshold {
        edges |= ResizeEdge::LEFT;
    }
    if point.x > frame.origin.x + frame.size.width - threshold {
        edges |= ResizeEdge::RIGHT;
    }
    if point.y < frame.origin.y + threshold {
        edges |= ResizeEdge::TOP;
    }
    if point.y > frame.origin.y + frame.size.height - threshold {
        edges |= ResizeEdge::BOTTOM;
    }
    // If both opposing edges are set (window too small for edge detection),
    // disable that axis to avoid conflicting resize directions.
    if edges & ResizeEdge::LEFT != 0 && edges & ResizeEdge::RIGHT != 0 {
        edges &= !(ResizeEdge::LEFT | ResizeEdge::RIGHT);
    }
    if edges & ResizeEdge::TOP != 0 && edges & ResizeEdge::BOTTOM != 0 {
        edges &= !(ResizeEdge::TOP | ResizeEdge::BOTTOM);
    }
    ResizeEdge(edges)
}

#[cfg(test)]
impl LayoutManager {
    pub(crate) fn new_for_test() -> Self {
        Self::new(default_config())
    }

    pub(crate) fn active_context_for_test(&self, space: SpaceId) -> ContextKey {
        self.active_context(space)
    }

    /// Whether a layout of the Space for `key`, of any screen size, has a
    /// node for the window.
    pub(crate) fn has_node_in(&self, space: SpaceId, key: ContextKey, wid: WindowId) -> bool {
        let Some(mapping) = self.mapping(space, key) else {
            return false;
        };
        mapping.layouts().any(|layout| self.tree.window_node(layout, wid).is_some())
    }
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::CGPoint;
    use pretty_assertions::assert_eq;
    use test_log::test;

    use super::*;
    use crate::config::WindowRuleConditions;
    use crate::model::contexts::Contexts;

    const EVERYTHING: ActiveContext = ActiveContext::EVERYTHING;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> CGRect {
        CGRect::new(CGPoint::new(x as f64, y as f64), CGSize::new(w as f64, h as f64))
    }

    fn make_windows(pid: pid_t, num: u32) -> Vec<(WindowId, LayoutWindowInfo)> {
        (1..=num).map(|idx| (WindowId::new(pid, idx), win_info())).collect()
    }

    fn win_info() -> LayoutWindowInfo {
        LayoutWindowInfo {
            frame: CGRect::ZERO,
            bundle_id: None,
            app_name: None,
            title: None,
            layer: Some(0),
            is_standard: true,
            is_resizable: true,
            ax_role: String::new(),
            ax_subrole: None,
        }
    }

    fn config_with_scroll(enable: bool, default_layout_kind: LayoutKind) -> Arc<Config> {
        let mut config = Config::default();
        config.settings.experimental.scroll.enable = enable;
        config.settings.default_layout_kind = default_layout_kind;
        Arc::new(config)
    }

    #[test]
    fn no_rules_falls_back_to_builtin_heuristics() {
        let mut info = win_info();
        assert_eq!(classify_window(&[], &info), WindowClass::Regular);
        info.is_standard = false;
        assert_eq!(classify_window(&[], &info), WindowClass::FloatByDefault);
    }

    #[test]
    fn app_id_rule_floats_matching_window_only() {
        let rules = [WindowRule {
            conditions: WindowRuleConditions {
                app_id: Some("com.example.X".into()),
                ..Default::default()
            },
            float: true,
        }];

        let mut info = win_info();
        info.bundle_id = Some("com.example.X".into());
        assert_eq!(classify_window(&rules, &info), WindowClass::FloatByDefault);

        info.bundle_id = Some("com.example.Y".into());
        assert_eq!(classify_window(&rules, &info), WindowClass::Regular);
    }

    #[test]
    fn earlier_rule_wins() {
        let rules = [
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
                    app_id: Some("com.example.X".into()),
                    ..Default::default()
                },
                float: false,
            },
        ];

        let mut info = win_info();
        info.bundle_id = Some("com.example.X".into());
        info.title = Some("My Dialog".to_owned().into());
        assert_eq!(classify_window(&rules, &info), WindowClass::FloatByDefault);

        info.title = Some("Main Window".to_owned().into());
        assert_eq!(classify_window(&rules, &info), WindowClass::Regular);
    }

    #[test]
    fn rule_conditions_combine_with_and() {
        let rules = [WindowRule {
            conditions: WindowRuleConditions {
                app_name: Some("Code".into()),
                title_substring: Some("Settings".into()),
                ax_role: Some("AXWindow".into()),
                ax_subrole: Some("AXDialog".into()),
                ..Default::default()
            },
            float: true,
        }];

        let mut info = win_info();
        info.app_name = Some("Visual Studio Code".into());
        info.title = Some("Settings — main".to_owned().into());
        info.ax_role = "AXWindow".into();
        info.ax_subrole = Some("AXDialog".into());
        assert_eq!(classify_window(&rules, &info), WindowClass::FloatByDefault);

        // One mismatched condition (subrole) means the rule does not apply.
        info.ax_subrole = Some("AXStandardWindow".into());
        assert_eq!(classify_window(&rules, &info), WindowClass::Regular);
    }

    #[test]
    fn rule_conditions_match_case_insensitively() {
        let rules = [WindowRule {
            conditions: WindowRuleConditions {
                app_id: Some("COM.EXAMPLE.x".into()),
                app_name: Some("code".into()),
                title_regex: Some("dialog".parse().unwrap()),
                title_substring: Some("SETTINGS".into()),
                ax_role: Some("axwindow".into()),
                ax_subrole: Some("AXDIALOG".into()),
            },
            float: true,
        }];

        let mut info = win_info();
        info.bundle_id = Some("com.example.X".into());
        info.app_name = Some("Visual Studio Code".into());
        info.title = Some("Settings Dialog".to_owned().into());
        info.ax_role = "AXWindow".into();
        info.ax_subrole = Some("AXDialog".into());
        assert_eq!(classify_window(&rules, &info), WindowClass::FloatByDefault);
    }

    #[test]
    fn condition_does_not_match_when_attribute_is_absent() {
        // A rule that requires an attribute must not match a window that lacks
        // it; the condition should fail rather than be skipped.
        let rules = [WindowRule {
            conditions: WindowRuleConditions {
                app_id: Some("com.example.X".into()),
                ..Default::default()
            },
            float: true,
        }];
        let mut info = win_info();
        info.bundle_id = None;
        assert_eq!(classify_window(&rules, &info), WindowClass::Regular);
    }

    #[test]
    fn rule_overrides_builtin_float_heuristics() {
        // System Preferences floats by default; a rule can force it to tile.
        let rules = [WindowRule {
            conditions: WindowRuleConditions {
                app_id: Some("com.apple.systempreferences".into()),
                ..Default::default()
            },
            float: false,
        }];
        let mut info = win_info();
        info.bundle_id = Some("com.apple.systempreferences".into());
        assert_eq!(classify_window(&rules, &info), WindowClass::Regular);

        // A non-resizable window floats by default; a rule can force it to tile.
        let rules = [WindowRule {
            conditions: WindowRuleConditions {
                app_id: Some("com.example.X".into()),
                ..Default::default()
            },
            float: false,
        }];
        let mut info = win_info();
        info.bundle_id = Some("com.example.X".into());
        info.is_resizable = false;
        assert_eq!(classify_window(&rules, &info), WindowClass::Regular);
    }

    #[test]
    fn rules_cannot_override_untracked_phantom_windows() {
        let rules = [WindowRule {
            conditions: WindowRuleConditions::default(),
            float: true,
        }];
        let mut info = win_info();
        info.layer = Some(3);
        assert_eq!(classify_window(&rules, &info), WindowClass::Untracked);
    }

    impl LayoutManager {
        fn layout_sorted(&self, space: SpaceId, screen: CGRect) -> Vec<(WindowId, CGRect)> {
            let mut layout = self.calculate_layout(
                space,
                screen,
                &Config::default(), // TODO stop being lazy
            );
            layout.sort_by_key(|(wid, _)| *wid);
            layout
        }
    }

    #[test]
    fn it_maintains_separate_layouts_for_each_screen_size() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let windows = make_windows(pid, 3);

        // Set up the starting layout.
        let screen1 = rect(0, 0, 120, 120);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 120, 60)),
                (WindowId::new(pid, 2), rect(0, 60, 60, 60)),
                (WindowId::new(pid, 3), rect(60, 60, 60, 60)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        // Introduce new screen size.
        let screen2 = rect(0, 0, 1200, 1200);
        _ = mgr.handle_event(SpaceExposed(space, screen2.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 1200, 600)),
                (WindowId::new(pid, 2), rect(0, 600, 600, 600)),
                (WindowId::new(pid, 3), rect(600, 600, 600, 600)),
            ],
            mgr.layout_sorted(space, screen2),
            "layout was not correctly scaled to new screen size"
        );

        // Change the layout for the second screen size.
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Down));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 400, 1200)),
                (WindowId::new(pid, 2), rect(400, 0, 400, 1200)),
                (WindowId::new(pid, 3), rect(800, 0, 400, 1200)),
            ],
            mgr.layout_sorted(space, screen2),
        );

        // Switch back to the first size; the layout should be the same as before.
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 120, 60)),
                (WindowId::new(pid, 2), rect(0, 60, 60, 60)),
                (WindowId::new(pid, 3), rect(60, 60, 60, 60)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        // Switch back to the second size.
        _ = mgr.handle_event(SpaceExposed(space, screen2.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 400, 1200)),
                (WindowId::new(pid, 2), rect(400, 0, 400, 1200)),
                (WindowId::new(pid, 3), rect(800, 0, 400, 1200)),
            ],
            mgr.layout_sorted(space, screen2),
        );
    }

    #[test]
    fn it_culls_unmodified_layouts() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let windows = make_windows(pid, 3);

        // Set up the starting layout but do not modify it.
        let screen1 = rect(0, 0, 120, 120);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 40, 120)),
                (WindowId::new(pid, 2), rect(40, 0, 40, 120)),
                (WindowId::new(pid, 3), rect(80, 0, 40, 120)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        // Introduce new screen size.
        let screen2 = rect(0, 0, 1200, 1200);
        _ = mgr.handle_event(SpaceExposed(space, screen2.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 400, 1200)),
                (WindowId::new(pid, 2), rect(400, 0, 400, 1200)),
                (WindowId::new(pid, 3), rect(800, 0, 400, 1200)),
            ],
            mgr.layout_sorted(space, screen2),
            "layout was not correctly scaled to new screen size"
        );

        // Change the layout for the second screen size.
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 1200, 600)),
                (WindowId::new(pid, 2), rect(0, 600, 600, 600)),
                (WindowId::new(pid, 3), rect(600, 600, 600, 600)),
            ],
            mgr.layout_sorted(space, screen2),
        );

        // Switch back to the first size. We should see a downscaled
        // version of the modified layout.
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 120, 60)),
                (WindowId::new(pid, 2), rect(0, 60, 60, 60)),
                (WindowId::new(pid, 3), rect(60, 60, 60, 60)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        // Switch to a third size. We should see a scaled version of the same.
        let screen3 = rect(0, 0, 12, 12);
        _ = mgr.handle_event(SpaceExposed(space, screen3.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 12, 6)),
                (WindowId::new(pid, 2), rect(0, 6, 6, 6)),
                (WindowId::new(pid, 3), rect(6, 6, 6, 6)),
            ],
            mgr.layout_sorted(space, screen3),
        );

        // Modify the layout.
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Left));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 6, 12)),
                (WindowId::new(pid, 2), rect(6, 0, 3, 12)),
                (WindowId::new(pid, 3), rect(9, 0, 3, 12)),
            ],
            mgr.layout_sorted(space, screen3),
        );

        // Switch back to the first size. We should see a scaled
        // version of the newly modified layout.
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 60, 120)),
                (WindowId::new(pid, 2), rect(60, 0, 30, 120)),
                (WindowId::new(pid, 3), rect(90, 0, 30, 120)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        // Modify the layout in the first size.
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Right));

        // Switch back to the second screen size, then the first, then the
        // second again. Since the layout was modified in the second size, the
        // windows should go back to the way they were laid out then.
        _ = mgr.handle_event(SpaceExposed(space, screen2.size, EVERYTHING));
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(SpaceExposed(space, screen2.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 1200, 600)),
                (WindowId::new(pid, 2), rect(0, 600, 600, 600)),
                (WindowId::new(pid, 3), rect(600, 600, 600, 600)),
            ],
            mgr.layout_sorted(space, screen2),
        );
    }

    #[test]
    fn floating_windows() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let config = &Config::default();

        let screen1 = rect(0, 0, 120, 120);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));

        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 2)));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        // Make the first window float.
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        let sizes: HashMap<_, _> =
            mgr.calculate_layout(space, screen1, config).into_iter().collect();
        assert_eq!(sizes[&WindowId::new(pid, 2)], rect(0, 0, 60, 120));
        assert_eq!(sizes[&WindowId::new(pid, 3)], rect(60, 0, 60, 120));

        // Toggle back to the tiled windows.
        let response = mgr.handle_command(Some(space), &[space], ToggleFocusFloating);
        assert_eq!(
            vec![WindowId::new(pid, 3), WindowId::new(pid, 2)],
            response.raise_windows
        );
        assert_eq!(Some(WindowId::new(pid, 2)), response.focus_window);
        if let Some(focus) = response.focus_window {
            _ = mgr.handle_event(WindowFocused(vec![space], focus));
        }

        // Make the second window float.
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        let sizes: HashMap<_, _> =
            mgr.calculate_layout(space, screen1, config).into_iter().collect();
        assert_eq!(sizes[&WindowId::new(pid, 3)], rect(0, 0, 120, 120));

        // Toggle back to tiled.
        let response = mgr.handle_command(Some(space), &[space], ToggleFocusFloating);
        assert_eq!(vec![WindowId::new(pid, 3)], response.raise_windows);
        assert_eq!(Some(WindowId::new(pid, 3)), response.focus_window);
        if let Some(focus) = response.focus_window {
            _ = mgr.handle_event(WindowFocused(vec![space], focus));
        }

        // Toggle back to floating.
        let response = mgr.handle_command(Some(space), &[space], ToggleFocusFloating);
        assert_eq!(vec![WindowId::new(pid, 1)], response.raise_windows);
        assert_eq!(Some(WindowId::new(pid, 2)), response.focus_window);
        if let Some(focus) = response.focus_window {
            _ = mgr.handle_event(WindowFocused(vec![space], focus));
        }
    }

    #[test]
    fn floating_windows_space_disabled() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let config = &Config::default();

        _ = mgr.handle_event(WindowFocused(vec![], WindowId::new(pid, 1)));

        // Make the first window float.
        _ = mgr.handle_command(None, &[], ToggleWindowFloating);

        // Enable the space.
        let screen1 = rect(0, 0, 120, 120);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));

        let sizes: HashMap<_, _> =
            mgr.calculate_layout(space, screen1, config).into_iter().collect();
        assert_eq!(sizes[&WindowId::new(pid, 2)], rect(0, 0, 60, 120));
        assert_eq!(sizes[&WindowId::new(pid, 3)], rect(60, 0, 60, 120));

        // Toggle back to the tiled windows.
        let response = mgr.handle_command(Some(space), &[space], ToggleFocusFloating);
        let mut raised_windows = response.raise_windows;
        raised_windows.extend(response.focus_window);
        raised_windows.sort();
        assert_eq!(
            vec![WindowId::new(pid, 2), WindowId::new(pid, 3)],
            raised_windows
        );
        // This if let is kind of load bearing for this test: previously we
        // allowed passing None for the window id of this event, except we
        // did that in the test but not in production. This led to an uncaught
        // bug!
        if let Some(focus) = response.focus_window {
            _ = mgr.handle_event(WindowFocused(vec![space], focus));
        }

        // Toggle back to floating.
        let response = mgr.handle_command(Some(space), &[space], ToggleFocusFloating);
        assert!(response.raise_windows.is_empty());
        assert_eq!(Some(WindowId::new(pid, 1)), response.focus_window);
        if let Some(focus) = response.focus_window {
            _ = mgr.handle_event(WindowFocused(vec![space], focus));
        }
    }

    #[test]
    fn space_exposed_does_not_bury_focused_floating_window() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;

        let screen1 = rect(0, 0, 120, 120);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        // With a tiled window focused, re-exposing the space (e.g. via mission
        // control) raises the tiled windows on top.
        let response = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        assert!(!response.raise_windows.is_empty());

        // Float the focused window and re-expose the space. The floating window
        // must not be buried, so no tiled windows are raised over it.
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        let response = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        assert!(
            response.raise_windows.is_empty()
                || response.raise_windows.contains(&WindowId::new(pid, 1))
        );
    }

    /// L11.
    #[test]
    fn l11_adding_a_window_that_has_a_node_in_the_layout_changes_nothing() {
        use LayoutEvent::*;
        let screen = rect(0, 0, 300, 30);
        for scroll in [false, true] {
            let mut mgr = LayoutManager::new_for_test();
            if scroll {
                mgr.set_config(&config_with_scroll(true, LayoutKind::Scroll));
            }
            let space = SpaceId::new(1);
            let pid = 1;
            _ = mgr.handle_event(SpaceExposed(space, screen.size, EVERYTHING));
            _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 2)));
            _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));
            let before = mgr.layout_sorted(space, screen);
            let tree = mgr.tree.draw_tree(mgr.layout(space));

            _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 2), win_info()));
            _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 1), win_info()));

            assert_eq!(before, mgr.layout_sorted(space, screen), "scroll: {scroll}");
            assert_eq!(tree, mgr.tree.draw_tree(mgr.layout(space)), "scroll: {scroll}");
        }
    }

    #[test]
    fn it_adds_new_windows_behind_selection() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let windows = make_windows(pid, 5);

        let screen1 = rect(0, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 5)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        _ = mgr.handle_command(Some(space), &[space], ToggleFocusFloating);
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 2)));
        _ = mgr.handle_command(Some(space), &[space], Split(Orientation::Vertical));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 3)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Left));

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 15)),
                (WindowId::new(pid, 3), rect(100, 15, 100, 15)),
                (WindowId::new(pid, 4), rect(200, 0, 100, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        // Add a new window when the left window is selected.
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 6), win_info()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 75, 30)),
                (WindowId::new(pid, 2), rect(150, 0, 75, 15)),
                (WindowId::new(pid, 3), rect(150, 15, 75, 15)),
                (WindowId::new(pid, 4), rect(225, 0, 75, 30)),
                (WindowId::new(pid, 6), rect(75, 0, 75, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );
        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 6)));

        // Add a new window when the top middle is selected.
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 2)));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 6), win_info()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 10)),
                (WindowId::new(pid, 3), rect(100, 20, 100, 10)),
                (WindowId::new(pid, 4), rect(200, 0, 100, 30)),
                (WindowId::new(pid, 6), rect(100, 10, 100, 10)),
            ],
            mgr.layout_sorted(space, screen1),
        );
        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 6)));

        // Same thing, but unfloat an existing window instead of making a new one.
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 2)));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 5)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 10)),
                (WindowId::new(pid, 3), rect(100, 20, 100, 10)),
                (WindowId::new(pid, 4), rect(200, 0, 100, 30)),
                (WindowId::new(pid, 5), rect(100, 10, 100, 10)),
            ],
            mgr.layout_sorted(space, screen1),
        );
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);

        // Add a new window when the bottom middle is selected.
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 3)));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 6), win_info()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 10)),
                (WindowId::new(pid, 3), rect(100, 10, 100, 10)),
                (WindowId::new(pid, 4), rect(200, 0, 100, 30)),
                (WindowId::new(pid, 6), rect(100, 20, 100, 10)),
            ],
            mgr.layout_sorted(space, screen1),
        );
        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 6)));

        // Add a new window when the right window is selected.
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 4)));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 6), win_info()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 75, 30)),
                (WindowId::new(pid, 2), rect(75, 0, 75, 15)),
                (WindowId::new(pid, 3), rect(75, 15, 75, 15)),
                (WindowId::new(pid, 4), rect(150, 0, 75, 30)),
                (WindowId::new(pid, 6), rect(225, 0, 75, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );
        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 6)));
    }

    #[test]
    fn add_remove_add() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;

        let screen1 = rect(0, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, vec![]));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 1), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 2), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 3), win_info()));

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 30)),
                (WindowId::new(pid, 3), rect(200, 0, 100, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 3)));
        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 1)));
        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 2)));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 1), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 2), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 3), win_info()));

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 30)),
                (WindowId::new(pid, 3), rect(200, 0, 100, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );
    }

    #[test]
    fn resize_to_full_screen_and_back_preserves_layout() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;

        let screen1 = rect(0, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, vec![]));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 1), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 2), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 3), win_info()));

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 30)),
                (WindowId::new(pid, 3), rect(200, 0, 100, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        _ = mgr.handle_event(WindowResized {
            wid: WindowId::new(pid, 2),
            old_frame: rect(100, 0, 100, 30),
            new_frame: screen1,
            screens: vec![(space, screen1)],
        });

        // Check that the other windows aren't resized (especially to zero);
        // otherwise, we will lose the layout state as we receive nonconforming
        // frame changed events.
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), screen1),
                (WindowId::new(pid, 3), rect(200, 0, 100, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        _ = mgr.handle_event(WindowResized {
            wid: WindowId::new(pid, 2),
            old_frame: screen1,
            new_frame: rect(100, 0, 100, 30),
            screens: vec![(space, screen1)],
        });

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 30)),
                (WindowId::new(pid, 3), rect(200, 0, 100, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );
    }

    #[test]
    fn resize_to_system_full_screen_and_back_preserves_layout() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;

        let screen1 = rect(0, 10, 300, 20);
        let screen1_full = rect(0, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, vec![]));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 1), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 2), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 3), win_info()));

        let orig = vec![
            (WindowId::new(pid, 1), rect(0, 10, 100, 20)),
            (WindowId::new(pid, 2), rect(100, 10, 100, 20)),
            (WindowId::new(pid, 3), rect(200, 10, 100, 20)),
        ];
        assert_eq!(orig, mgr.layout_sorted(space, screen1));

        // Simulate a window going fullscreen on the current space.
        //
        // The leftmost window is better for testing because it passes the
        // "only resize in 2 directions" requirement.
        _ = mgr.handle_event(WindowResized {
            wid: WindowId::new(pid, 1),
            old_frame: rect(0, 10, 100, 20),
            new_frame: screen1_full,
            screens: vec![(space, screen1)],
        });

        _ = mgr.handle_event(WindowResized {
            wid: WindowId::new(pid, 1),
            old_frame: screen1_full,
            new_frame: rect(0, 10, 100, 20),
            screens: vec![(space, screen1)],
        });

        assert_eq!(orig, mgr.layout_sorted(space, screen1));
    }

    #[test]
    fn flip_between_screens() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let pid = 1;

        let screen1 = rect(0, 0, 300, 30);
        let screen2 = rect(300, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space1, screen1.size, EVERYTHING));
        _ = mgr.handle_event(SpaceExposed(space2, screen2.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(
            space1,
            pid,
            vec![
                (WindowId::new(pid, 1), win_info()),
                (WindowId::new(pid, 2), win_info()),
            ],
        ));
        _ = mgr.handle_event(WindowsOnScreenUpdated(
            space2,
            pid,
            vec![
                (WindowId::new(pid, 3), win_info()),
                (WindowId::new(pid, 4), win_info()),
            ],
        ));
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], WindowId::new(pid, 3)));
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], WindowId::new(pid, 1)));

        // Test moving focus between screens.
        assert_eq!(
            mgr.handle_command(Some(space1), &[space1, space2], MoveFocus(Direction::Right))
                .focus_window,
            Some(WindowId::new(pid, 2))
        );
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], WindowId::new(pid, 2)));
        assert_eq!(
            mgr.handle_command(Some(space1), &[space1, space2], MoveFocus(Direction::Right))
                .focus_window,
            Some(WindowId::new(pid, 3))
        );
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], WindowId::new(pid, 3)));
        assert_eq!(
            mgr.handle_command(Some(space2), &[space1, space2], MoveFocus(Direction::Left))
                .focus_window,
            Some(WindowId::new(pid, 2))
        );
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], WindowId::new(pid, 3)));

        // Test moving a node between screens.
        _ = mgr.handle_command(Some(space1), &[space1, space2], MoveNode(Direction::Right));
        mgr.debug_tree(space2);
        assert_eq!(
            vec![(WindowId::new(pid, 1), rect(0, 0, 300, 30)),],
            mgr.layout_sorted(space1, screen1),
        );
        assert_eq!(
            vec![
                // Note that 2 is moved to the right of 3.
                (WindowId::new(pid, 2), rect(400, 0, 100, 30)),
                (WindowId::new(pid, 3), rect(300, 0, 100, 30)),
                (WindowId::new(pid, 4), rect(500, 0, 100, 30)),
            ],
            mgr.layout_sorted(space2, screen2),
        );
        assert_eq!(Some(WindowId::new(pid, 2)), mgr.selected_window(space2));

        // Finally, test moving focus after moving the node.
        assert_eq!(
            mgr.handle_command(Some(space2), &[space1, space2], MoveFocus(Direction::Right))
                .focus_window,
            Some(WindowId::new(pid, 4))
        );
    }

    #[test]
    fn move_node_between_spaces_keeps_window_mapping_consistent() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let pid = 1;

        let screen1 = rect(0, 0, 300, 30);
        let screen2 = rect(300, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space1, screen1.size, EVERYTHING));
        _ = mgr.handle_event(SpaceExposed(space2, screen2.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(
            space1,
            pid,
            vec![
                (WindowId::new(pid, 1), win_info()),
                (WindowId::new(pid, 2), win_info()),
            ],
        ));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space2, pid, vec![]));

        let moved = WindowId::new(pid, 2);
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], moved));

        // Move the selected window off the right edge of space1, into space2.
        _ = mgr.handle_command(Some(space1), &[space1, space2], MoveNode(Direction::Right));

        let windows_in = |mgr: &LayoutManager, space, screen| {
            mgr.layout_sorted(space, screen)
                .into_iter()
                .map(|(wid, _)| wid)
                .collect::<Vec<_>>()
        };
        assert!(!windows_in(&mgr, space1, screen1).contains(&moved));
        assert!(windows_in(&mgr, space2, screen2).contains(&moved));

        // The window <-> layout mapping must follow the node into space2. The
        // old move_node_after reparented the node but left this mapping pointing
        // at space1, so window_node lookups returned the wrong layout. This is
        // the long-standing bug this assertion guards against.
        assert_eq!(None, mgr.tree.window_node(mgr.layout(space1), moved));
        assert!(mgr.tree.window_node(mgr.layout(space2), moved).is_some());

        // A follow-up that resolves the window through its mapping now works.
        // Focusing it must select it in space2 (WindowFocused uses window_node),
        // and moving it left must carry it back to space1. With a stale mapping
        // the focus would miss and the window would never return to space1.
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], moved));
        _ = mgr.handle_command(Some(space2), &[space1, space2], MoveNode(Direction::Left));
        assert!(windows_in(&mgr, space1, screen1).contains(&moved));
        assert!(!windows_in(&mgr, space2, screen2).contains(&moved));
    }

    #[test]
    fn floating_window_space_change_stays_floating() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let pid = 1;

        let screen1 = rect(0, 0, 300, 30);
        let screen2 = rect(300, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space1, screen1.size, EVERYTHING));
        _ = mgr.handle_event(SpaceExposed(space2, screen2.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space1, pid, make_windows(pid, 2)));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space2, pid, vec![]));

        // Float the first window on space1.
        let floated = WindowId::new(pid, 1);
        _ = mgr.handle_event(WindowFocused(vec![space1], floated));
        _ = mgr.handle_command(Some(space1), &[space1], ToggleWindowFloating);
        assert_eq!(BTreeSet::from([floated]), mgr.floating_windows_in_space(space1));

        // Drag the floating window onto space2.
        _ = mgr.handle_event(WindowSpaceChanged {
            wid: floated,
            added: Some(space2),
            removed: Some(space1),
            info: win_info(),
            contexts_in_use: false,
        });

        // It must remain floating, tracked under space2 and not space1. It must
        // not be tiled into either layout: the old handler called
        // add_window_after unconditionally, which pulled it into space2's tree.
        assert_eq!(BTreeSet::new(), mgr.floating_windows_in_space(space1));
        assert_eq!(BTreeSet::from([floated]), mgr.floating_windows_in_space(space2));
        let tiled = |mgr: &LayoutManager, space, screen| {
            mgr.layout_sorted(space, screen)
                .into_iter()
                .map(|(wid, _)| wid)
                .collect::<Vec<_>>()
        };
        assert!(!tiled(&mgr, space1, screen1).contains(&floated));
        assert!(!tiled(&mgr, space2, screen2).contains(&floated));
    }

    #[test]
    fn window_entering_a_space_for_the_first_time_is_classified() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;

        let screen = rect(0, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space, screen.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 2)));

        // A window that was off screen when we first saw it never went through
        // classification, so a space change is its first layout event.
        let nonstandard = WindowId::new(pid, 3);
        _ = mgr.handle_event(WindowSpaceChanged {
            wid: nonstandard,
            added: Some(space),
            removed: None,
            info: LayoutWindowInfo {
                is_standard: false,
                ..win_info()
            },
            contexts_in_use: false,
        });

        assert_eq!(
            BTreeSet::from([nonstandard]),
            mgr.floating_windows_in_space(space)
        );
        let tiled = mgr
            .layout_sorted(space, screen)
            .into_iter()
            .map(|(wid, _)| wid)
            .collect::<Vec<_>>();
        assert!(!tiled.contains(&nonstandard));
    }

    #[test]
    fn window_dragged_into_scroll_space_joins_a_column() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        mgr.set_config(&config_with_scroll(true, LayoutKind::Scroll));
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let pid = 1;

        let screen1 = rect(0, 0, 300, 30);
        let screen2 = rect(300, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space1, screen1.size, EVERYTHING));
        _ = mgr.handle_event(SpaceExposed(space2, screen2.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space1, pid, make_windows(pid, 2)));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space2, pid, vec![]));
        assert_eq!(LayoutKind::Scroll, mgr.active_layout_kind(space2));

        // Drag a window from the scroll space1 into the scroll space2.
        let moved = WindowId::new(pid, 1);
        _ = mgr.handle_event(WindowSpaceChanged {
            wid: moved,
            added: Some(space2),
            removed: Some(space1),
            info: win_info(),
            contexts_in_use: false,
        });

        // In a scroll layout windows live inside column containers, never as a
        // direct child of the root. The old handler called add_window_after,
        // which dropped a bare window node directly under the root.
        let layout2 = mgr.layout(space2);
        let node = mgr.tree.window_node(layout2, moved).expect("window should be in space2");
        assert_ne!(
            Some(mgr.tree.root(layout2)),
            node.parent(mgr.tree.map()),
            "scroll-space window should be nested in a column, not under the root"
        );
    }

    #[test]
    fn focus_next_prev() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;

        let screen = rect(0, 0, 1000, 1000);
        _ = mgr.handle_event(SpaceExposed(space, screen.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(
            space,
            pid,
            vec![
                (WindowId::new(pid, 1), win_info()),
                (WindowId::new(pid, 2), win_info()),
                (WindowId::new(pid, 3), win_info()),
            ],
        ));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        // Test FocusNext
        assert_eq!(
            mgr.handle_command(Some(space), &[space], FocusNext).focus_window,
            Some(WindowId::new(pid, 2))
        );
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 2)));

        assert_eq!(
            mgr.handle_command(Some(space), &[space], FocusNext).focus_window,
            Some(WindowId::new(pid, 3))
        );
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 3)));

        assert_eq!(
            mgr.handle_command(Some(space), &[space], FocusNext).focus_window,
            Some(WindowId::new(pid, 1))
        ); // wraparound
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        // Test FocusPrev
        assert_eq!(
            mgr.handle_command(Some(space), &[space], FocusPrev).focus_window,
            Some(WindowId::new(pid, 3))
        ); // wraparound
    }

    #[test]
    fn it_resizes_windows_with_resize_command() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let windows = make_windows(pid, 2);

        let screen = rect(0, 0, 100, 100);
        _ = mgr.handle_event(SpaceExposed(space, screen.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 50, 100)),
                (WindowId::new(pid, 2), rect(50, 0, 50, 100)),
            ],
            mgr.layout_sorted(space, screen),
        );

        _ = mgr.handle_command(
            Some(space),
            &[space],
            Resize {
                direction: Direction::Right,
                percent: 10.0,
            },
        );
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 60, 100)),
                (WindowId::new(pid, 2), rect(60, 0, 40, 100)),
            ],
            mgr.layout_sorted(space, screen),
        );

        _ = mgr.handle_command(Some(space), &[space], MoveFocus(Direction::Right));
        _ = mgr.handle_command(
            Some(space),
            &[space],
            Resize {
                direction: Direction::Left,
                percent: 10.0,
            },
        );
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 50, 100)),
                (WindowId::new(pid, 2), rect(50, 0, 50, 100)),
            ],
            mgr.layout_sorted(space, screen),
        );
    }

    #[test]
    fn a_keyboard_resize_releases_a_size_share_lock() {
        let (mut mgr, space, screen) = setup_size_share_test(2);
        _ = set_size_share(&mut mgr, space, SizeShare::Fraction(0.5));
        assert_eq!(
            vec![
                (WindowId::new(1, 1), rect(0, 0, 600, 1200)),
                (WindowId::new(1, 2), rect(600, 0, 600, 1200)),
            ],
            mgr.layout_sorted(space, screen),
        );

        // The focused window is locked; resizing it unfreezes it.
        _ = mgr.handle_command(
            Some(space),
            &[space],
            LayoutCommand::Resize {
                direction: Direction::Right,
                percent: 10.0,
            },
        );
        assert!(mgr.tree.size_lock(WindowId::new(1, 1)).is_none());
        assert_eq!(
            vec![
                (WindowId::new(1, 1), rect(0, 0, 720, 1200)),
                (WindowId::new(1, 2), rect(720, 0, 480, 1200)),
            ],
            mgr.layout_sorted(space, screen),
        );
    }

    #[test]
    fn a_keyboard_resize_leaves_a_neighboring_size_lock_alone() {
        let (mut mgr, space, screen) = setup_size_share_test(2);
        _ = set_size_share(&mut mgr, space, SizeShare::Fraction(0.5));
        _ = mgr.handle_event(LayoutEvent::WindowFocused(vec![space], WindowId::new(1, 2)));

        // Window 2 resizes away from window 1, whose share stays locked.
        _ = mgr.handle_command(
            Some(space),
            &[space],
            LayoutCommand::Resize {
                direction: Direction::Right,
                percent: 10.0,
            },
        );
        assert_eq!(mgr.tree.size_lock(WindowId::new(1, 1)), Some(0.5));
        assert_eq!(
            vec![
                (WindowId::new(1, 1), rect(0, 0, 600, 1200)),
                (WindowId::new(1, 2), rect(600, 0, 600, 1200)),
            ],
            mgr.layout_sorted(space, screen),
        );

        // Resizing toward the locked window does nothing: that boundary is
        // frozen.
        _ = mgr.handle_command(
            Some(space),
            &[space],
            LayoutCommand::Resize {
                direction: Direction::Left,
                percent: 10.0,
            },
        );
        assert_eq!(
            vec![
                (WindowId::new(1, 1), rect(0, 0, 600, 1200)),
                (WindowId::new(1, 2), rect(600, 0, 600, 1200)),
            ],
            mgr.layout_sorted(space, screen),
        );
    }

    #[test]
    fn it_flips_the_container_with_toggle_orientation() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let windows = make_windows(pid, 2);

        let screen = rect(0, 0, 100, 100);
        _ = mgr.handle_event(SpaceExposed(space, screen.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 50, 100)),
                (WindowId::new(pid, 2), rect(50, 0, 50, 100)),
            ],
            mgr.layout_sorted(space, screen),
        );

        // Side by side becomes stacked top to bottom.
        _ = mgr.handle_command(Some(space), &[space], ToggleOrientation);
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 50)),
                (WindowId::new(pid, 2), rect(0, 50, 100, 50)),
            ],
            mgr.layout_sorted(space, screen),
        );

        // Toggling again returns to the original layout.
        _ = mgr.handle_command(Some(space), &[space], ToggleOrientation);
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 50, 100)),
                (WindowId::new(pid, 2), rect(50, 0, 50, 100)),
            ],
            mgr.layout_sorted(space, screen),
        );
    }

    #[test]
    fn toggle_orientation_keeps_a_group_a_group() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let windows = make_windows(pid, 2);

        let screen = rect(0, 0, 100, 100);
        _ = mgr.handle_event(SpaceExposed(space, screen.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        _ = mgr.handle_command(Some(space), &[space], Group(Orientation::Horizontal));
        _ = mgr.handle_command(Some(space), &[space], ToggleOrientation);

        let layout = mgr.layout(space);
        let parent = mgr.tree.selection(layout).parent(mgr.tree.map()).unwrap();
        assert_eq!(ContainerKind::Stacked, mgr.tree.container_kind(parent));
    }

    #[test]
    fn space_exposed_forces_tree_when_scroll_gate_disabled() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let config = config_with_scroll(false, LayoutKind::Scroll);
        mgr.set_config(&config);

        let space = SpaceId::new(1);
        _ = mgr.handle_event(SpaceExposed(space, rect(0, 0, 300, 200).size, EVERYTHING));

        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);
    }

    #[test]
    fn change_layout_kind_noops_when_scroll_gate_disabled() {
        use LayoutCommand::*;
        use LayoutEvent::*;

        let mut mgr = LayoutManager::new_for_test();
        let config = config_with_scroll(false, LayoutKind::Tree);
        mgr.set_config(&config);

        let space = SpaceId::new(1);
        let pid = 1;
        _ = mgr.handle_event(SpaceExposed(space, rect(0, 0, 400, 200).size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 2)));
        _ = mgr.handle_command(Some(space), &[space], ChangeLayoutKind);

        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);
    }

    #[test]
    fn active_scroll_layout_converts_to_tree_when_gate_disabled() {
        use LayoutEvent::*;

        let mut mgr = LayoutManager::new_for_test();
        let config_on = config_with_scroll(true, LayoutKind::Scroll);
        mgr.set_config(&config_on);

        let space = SpaceId::new(1);
        let pid = 1;
        _ = mgr.handle_event(SpaceExposed(space, rect(0, 0, 500, 300).size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Scroll);

        let config_off = config_with_scroll(false, LayoutKind::Tree);
        mgr.set_config(&config_off);

        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);
    }

    #[test]
    fn next_layout_skips_scroll_when_gate_disabled() {
        use LayoutCommand::*;
        use LayoutEvent::*;

        let mut mgr = LayoutManager::new_for_test();
        let config_on = config_with_scroll(true, LayoutKind::Scroll);
        mgr.set_config(&config_on);

        let space = SpaceId::new(1);
        let pid = 1;
        _ = mgr.handle_event(SpaceExposed(space, rect(0, 0, 500, 300).size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Scroll);

        _ = mgr.handle_command(Some(space), &[space], ChangeLayoutKind);
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);

        let config_off = config_with_scroll(false, LayoutKind::Tree);
        mgr.set_config(&config_off);
        _ = mgr.handle_command(Some(space), &[space], NextLayout);

        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);
    }

    #[test]
    fn scroll_wheel_is_ignored_when_scroll_gate_disabled() {
        use LayoutEvent::*;

        let mut mgr = LayoutManager::new_for_test();
        let config_on = config_with_scroll(true, LayoutKind::Scroll);
        mgr.set_config(&config_on);

        let space = SpaceId::new(1);
        let screen = rect(0, 0, 900, 600);
        let pid = 1;
        _ = mgr.handle_event(SpaceExposed(space, screen.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Scroll);

        let config_off = config_with_scroll(false, LayoutKind::Tree);
        mgr.set_config(&config_off);
        let response =
            mgr.handle_scroll_wheel(space, -1.0, &screen, &config_off.settings.experimental.scroll);
        assert!(response.raise_windows.is_empty());
        assert!(response.focus_window.is_none());
    }

    #[test]
    fn scroll_only_commands_noop_when_gate_disabled() {
        use LayoutCommand::*;
        use LayoutEvent::*;

        let mut mgr = LayoutManager::new_for_test();
        let config_on = config_with_scroll(true, LayoutKind::Scroll);
        mgr.set_config(&config_on);

        let space = SpaceId::new(1);
        let screen = rect(0, 0, 1000, 600);
        let pid = 1;
        _ = mgr.handle_event(SpaceExposed(space, screen.size, EVERYTHING));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Scroll);

        let config_off = config_with_scroll(false, LayoutKind::Tree);
        mgr.set_config(&config_off);
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);

        let before = mgr.layout_sorted(space, screen);
        _ = mgr.handle_command(Some(space), &[space], CycleColumnWidth);
        _ = mgr.handle_command(Some(space), &[space], ToggleColumnTabbed);
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);
        assert_eq!(mgr.layout_sorted(space, screen), before);
    }

    #[test]
    fn event_response_coalesce_downgrades_extra_focus_to_raise() {
        let response = EventResponse {
            frame_overrides: vec![],
            raise_windows: vec![WindowId::new(1, 1)],
            focus_window: Some(WindowId::new(1, 2)),
            ..Default::default()
        }
        .coalesce(EventResponse {
            frame_overrides: vec![],
            raise_windows: vec![WindowId::new(1, 3)],
            focus_window: Some(WindowId::new(1, 4)),
            ..Default::default()
        });

        assert_eq!(
            response.raise_windows,
            vec![
                WindowId::new(1, 1),
                WindowId::new(1, 3),
                WindowId::new(1, 4)
            ]
        );
        assert_eq!(response.focus_window, Some(WindowId::new(1, 2)));
    }

    // Tests for drag-to-rearrange zone detection

    #[test]
    fn drop_zone_center_detection() {
        let frame = rect(0, 0, 100, 100);
        let edge_ratio = 0.15;

        // Center of the frame should be Center zone
        let center = CGPoint::new(50.0, 50.0);
        assert_eq!(
            DropZoneRegion::from_point(center, frame, edge_ratio),
            DropZoneRegion::Center
        );

        // Just inside the center area
        let near_center = CGPoint::new(30.0, 30.0);
        assert_eq!(
            DropZoneRegion::from_point(near_center, frame, edge_ratio),
            DropZoneRegion::Center
        );
    }

    #[test]
    fn drop_zone_edge_detection() {
        let frame = rect(0, 0, 100, 100);
        let edge_ratio = 0.15;

        // Left edge
        let left = CGPoint::new(5.0, 50.0);
        assert_eq!(
            DropZoneRegion::from_point(left, frame, edge_ratio),
            DropZoneRegion::Left
        );

        // Right edge
        let right = CGPoint::new(95.0, 50.0);
        assert_eq!(
            DropZoneRegion::from_point(right, frame, edge_ratio),
            DropZoneRegion::Right
        );

        // Top edge (disabled, falls through to center)
        let top = CGPoint::new(50.0, 5.0);
        assert_eq!(
            DropZoneRegion::from_point(top, frame, edge_ratio),
            DropZoneRegion::Center
        );

        // Bottom edge
        let bottom = CGPoint::new(50.0, 95.0);
        assert_eq!(
            DropZoneRegion::from_point(bottom, frame, edge_ratio),
            DropZoneRegion::Bottom
        );
    }

    #[test]
    fn drop_zone_corner_picks_deeper_edge() {
        let frame = rect(0, 0, 100, 100);
        let edge_ratio = 0.15;

        // Top-left corner, more to the left -> Left
        let top_left_x = CGPoint::new(5.0, 10.0);
        assert_eq!(
            DropZoneRegion::from_point(top_left_x, frame, edge_ratio),
            DropZoneRegion::Left
        );

        // Top-left corner, more to the top -> Left (top zone disabled)
        let top_left_y = CGPoint::new(10.0, 5.0);
        assert_eq!(
            DropZoneRegion::from_point(top_left_y, frame, edge_ratio),
            DropZoneRegion::Left
        );
    }

    #[test]
    fn drop_zone_to_action_same_orientation() {
        use crate::model::NodeId;

        let node = NodeId::from(slotmap::KeyData::from_ffi(1));

        // Horizontal parent, left zone -> Insert before
        let action = DropZoneRegion::Left.to_action(node, Orientation::Horizontal);
        assert!(matches!(action, DropAction::Insert { before: true, .. }));

        // Horizontal parent, right zone -> Insert after
        let action = DropZoneRegion::Right.to_action(node, Orientation::Horizontal);
        assert!(matches!(action, DropAction::Insert { before: false, .. }));

        // Vertical parent, top zone -> Insert before
        let action = DropZoneRegion::Top.to_action(node, Orientation::Vertical);
        assert!(matches!(action, DropAction::Insert { before: true, .. }));

        // Vertical parent, bottom zone -> Insert after
        let action = DropZoneRegion::Bottom.to_action(node, Orientation::Vertical);
        assert!(matches!(action, DropAction::Insert { before: false, .. }));
    }

    #[test]
    fn drop_zone_to_action_cross_orientation_splits() {
        use crate::model::NodeId;

        let node = NodeId::from(slotmap::KeyData::from_ffi(1));

        // Vertical parent, left zone -> Split horizontal
        let action = DropZoneRegion::Left.to_action(node, Orientation::Vertical);
        assert!(matches!(
            action,
            DropAction::Split {
                orientation: ContainerKind::Horizontal,
                ..
            }
        ));

        // Vertical parent, right zone -> Split horizontal
        let action = DropZoneRegion::Right.to_action(node, Orientation::Vertical);
        assert!(matches!(
            action,
            DropAction::Split {
                orientation: ContainerKind::Horizontal,
                ..
            }
        ));

        // Horizontal parent, top zone -> Split vertical
        let action = DropZoneRegion::Top.to_action(node, Orientation::Horizontal);
        assert!(matches!(
            action,
            DropAction::Split {
                orientation: ContainerKind::Vertical,
                ..
            }
        ));

        // Horizontal parent, bottom zone -> Split vertical
        let action = DropZoneRegion::Bottom.to_action(node, Orientation::Horizontal);
        assert!(matches!(
            action,
            DropAction::Split {
                orientation: ContainerKind::Vertical,
                ..
            }
        ));
    }

    #[test]
    fn drop_zone_center_always_swap() {
        use crate::model::NodeId;

        let node = NodeId::from(slotmap::KeyData::from_ffi(1));

        // Center zone always results in swap, regardless of orientation
        let action = DropZoneRegion::Center.to_action(node, Orientation::Horizontal);
        assert!(matches!(action, DropAction::Swap { .. }));

        let action = DropZoneRegion::Center.to_action(node, Orientation::Vertical);
        assert!(matches!(action, DropAction::Swap { .. }));
    }

    /// Sets up a manager on one space with `num` windows and returns it.
    fn setup_size_share_test(num: u32) -> (LayoutManager, SpaceId, CGRect) {
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let screen = rect(0, 0, 1200, 1200);
        _ = mgr.handle_event(LayoutEvent::SpaceExposed(space, screen.size, EVERYTHING));
        _ = mgr.handle_event(LayoutEvent::WindowsOnScreenUpdated(
            space,
            pid,
            make_windows(pid, num),
        ));
        _ = mgr.handle_event(LayoutEvent::WindowFocused(vec![space], WindowId::new(pid, 1)));
        (mgr, space, screen)
    }

    fn set_size_share(
        mgr: &mut LayoutManager,
        space: SpaceId,
        share: SizeShare,
    ) -> Option<SizeShareFeedback> {
        mgr.handle_command(Some(space), &[space], LayoutCommand::SetSizeShare(share))
            .size_share_feedback
    }

    #[test]
    fn it_locks_and_releases_a_size_share() {
        use SizeShareOutcome::*;
        let (mut mgr, space, screen) = setup_size_share_test(3);
        let before = mgr.layout_sorted(space, screen);

        let feedback = set_size_share(&mut mgr, space, SizeShare::Fraction(0.5));
        assert_eq!(
            feedback,
            Some(SizeShareFeedback {
                wid: WindowId::new(1, 1),
                share: 0.5,
                outcome: Applied,
            })
        );
        assert_eq!(
            vec![
                (WindowId::new(1, 1), rect(0, 0, 600, 1200)),
                (WindowId::new(1, 2), rect(600, 0, 300, 1200)),
                (WindowId::new(1, 3), rect(900, 0, 300, 1200)),
            ],
            mgr.layout_sorted(space, screen),
        );

        // The same share again releases the lock and restores the layout.
        let feedback = set_size_share(&mut mgr, space, SizeShare::Fraction(0.5));
        assert_eq!(feedback.map(|f| f.outcome), Some(Released));
        assert_eq!(before, mgr.layout_sorted(space, screen));
    }

    #[test]
    fn it_locks_a_denominator_size_share() {
        let (mut mgr, space, screen) = setup_size_share_test(3);
        let feedback = set_size_share(&mut mgr, space, SizeShare::Denominator { denominator: 3 });
        assert_eq!(feedback.map(|f| f.share), Some(1.0 / 3.0));
        assert_eq!(
            vec![
                (WindowId::new(1, 1), rect(0, 0, 400, 1200)),
                (WindowId::new(1, 2), rect(400, 0, 400, 1200)),
                (WindowId::new(1, 3), rect(800, 0, 400, 1200)),
            ],
            mgr.layout_sorted(space, screen),
        );
    }

    #[test]
    fn it_rejects_size_shares_that_do_not_fit() {
        use SizeShareOutcome::*;
        let (mut mgr, space, screen) = setup_size_share_test(3);
        assert_eq!(
            set_size_share(&mut mgr, space, SizeShare::Fraction(0.5)).map(|f| f.outcome),
            Some(Applied)
        );
        _ = mgr.handle_event(LayoutEvent::WindowFocused(vec![space], WindowId::new(1, 2)));
        assert_eq!(
            set_size_share(&mut mgr, space, SizeShare::Fraction(0.5)).map(|f| f.outcome),
            Some(Applied)
        );
        let locked = mgr.layout_sorted(space, screen);

        _ = mgr.handle_event(LayoutEvent::WindowFocused(vec![space], WindowId::new(1, 3)));
        assert_eq!(
            set_size_share(&mut mgr, space, SizeShare::Fraction(0.25)).map(|f| f.outcome),
            Some(Rejected)
        );
        assert_eq!(locked, mgr.layout_sorted(space, screen));
    }

    #[test]
    fn it_squeezes_size_shares_when_overflow_is_allowed() {
        let mut config = Config::default();
        config.settings.size_share.overflow = SizeShareOverflow::Squeeze;
        let mut mgr = LayoutManager::new(Arc::new(config));
        let space = SpaceId::new(1);
        let pid = 1;
        let screen = rect(0, 0, 1200, 1200);
        _ = mgr.handle_event(LayoutEvent::SpaceExposed(space, screen.size, EVERYTHING));
        _ = mgr.handle_event(LayoutEvent::WindowsOnScreenUpdated(
            space,
            pid,
            make_windows(pid, 3),
        ));
        _ = mgr.handle_event(LayoutEvent::WindowFocused(vec![space], WindowId::new(pid, 1)));

        assert_eq!(
            set_size_share(&mut mgr, space, SizeShare::Fraction(0.5)).map(|f| f.outcome),
            Some(SizeShareOutcome::Applied)
        );
        _ = mgr.handle_event(LayoutEvent::WindowFocused(vec![space], WindowId::new(1, 2)));
        assert_eq!(
            set_size_share(&mut mgr, space, SizeShare::Fraction(0.5)).map(|f| f.outcome),
            Some(SizeShareOutcome::Applied)
        );
        _ = mgr.handle_event(LayoutEvent::WindowFocused(vec![space], WindowId::new(1, 3)));
        assert_eq!(
            set_size_share(&mut mgr, space, SizeShare::Fraction(0.25)).map(|f| f.outcome),
            Some(SizeShareOutcome::Applied)
        );

        // Locks sum to 1.25, so they scale down together to make it fit.
        assert_eq!(
            vec![
                (WindowId::new(1, 1), rect(0, 0, 480, 1200)),
                (WindowId::new(1, 2), rect(480, 0, 480, 1200)),
                (WindowId::new(1, 3), rect(960, 0, 240, 1200)),
            ],
            mgr.layout_sorted(space, screen),
        );
    }

    #[test]
    fn it_clears_size_shares_on_clean_up_space() {
        let (mut mgr, space, screen) = setup_size_share_test(3);
        let before = mgr.layout_sorted(space, screen);

        _ = set_size_share(&mut mgr, space, SizeShare::Fraction(0.5));
        assert!(mgr.tree.size_lock(WindowId::new(1, 1)).is_some());
        _ = mgr.handle_command(Some(space), &[space], LayoutCommand::CleanUpSpace);
        assert!(mgr.tree.size_lock(WindowId::new(1, 1)).is_none());
        assert_eq!(before, mgr.layout_sorted(space, screen));
    }

    #[test]
    fn it_clears_size_shares_when_a_window_closes() {
        let (mut mgr, space, screen) = setup_size_share_test(2);
        _ = set_size_share(&mut mgr, space, SizeShare::Fraction(0.5));
        assert!(mgr.tree.size_lock(WindowId::new(1, 1)).is_some());

        _ = mgr.handle_event(LayoutEvent::WindowRemoved(WindowId::new(1, 1)));
        assert!(mgr.tree.size_lock(WindowId::new(1, 1)).is_none());
        assert_eq!(
            vec![(WindowId::new(1, 2), rect(0, 0, 1200, 1200))],
            mgr.layout_sorted(space, screen),
        );
    }

    #[test]
    fn it_releases_size_shares_on_user_resize() {
        let (mut mgr, space, _screen) = setup_size_share_test(2);
        _ = set_size_share(&mut mgr, space, SizeShare::Fraction(0.5));
        assert!(mgr.tree.size_lock(WindowId::new(1, 1)).is_some());

        assert!(mgr.release_size_share(WindowId::new(1, 1)));
        assert!(mgr.tree.size_lock(WindowId::new(1, 1)).is_none());
        assert!(!mgr.release_size_share(WindowId::new(1, 1)));
    }

    #[test]
    fn invalid_size_shares_are_ignored() {
        let (mut mgr, space, _screen) = setup_size_share_test(2);
        assert_eq!(set_size_share(&mut mgr, space, SizeShare::Fraction(0.0)), None);
        assert_eq!(set_size_share(&mut mgr, space, SizeShare::Fraction(1.0)), None);
        assert_eq!(set_size_share(&mut mgr, space, SizeShare::Fraction(1.5)), None);
        assert_eq!(
            set_size_share(&mut mgr, space, SizeShare::Denominator { denominator: 0 }),
            None
        );
    }

    fn named_contexts<const N: usize>(names: [&str; N]) -> [ContextKey; N] {
        let mut contexts = Contexts::new();
        names.map(|name| ContextKey::Named(contexts.create(name).unwrap()))
    }

    /// Makes `key` the Space's active context and sends the windows that show
    /// under it, as a switch does. All windows belong to app 1.
    fn switch(
        mgr: &mut LayoutManager,
        space: SpaceId,
        size: CGSize,
        key: ContextKey,
        showing: &[WindowId],
    ) {
        let context = ActiveContext {
            key,
            members: showing.iter().copied().collect(),
        };
        _ = mgr.handle_event(LayoutEvent::SpaceExposed(space, size, context));
        let windows = showing.iter().map(|&wid| (wid, win_info())).collect();
        _ = mgr.handle_event(LayoutEvent::WindowsOnScreenUpdated(space, 1, windows));
    }

    /// Counts the errors logged while `f` runs.
    fn count_errors(f: impl FnOnce()) -> usize {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use tracing_subscriber::layer::{Context, SubscriberExt};

        struct ErrorCounter(Arc<AtomicUsize>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ErrorCounter {
            fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
                if *event.metadata().level() == tracing::Level::ERROR {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        let count = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry().with(ErrorCounter(count.clone()));
        tracing::subscriber::with_default(subscriber, f);
        count.load(Ordering::Relaxed)
    }

    /// L3.
    #[test]
    fn a_context_layout_starts_as_the_shown_layout_without_non_members() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c] = named_contexts(["C"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        _ = mgr.handle_event(WindowFocused(vec![space], w(1)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        let everything = vec![
            (w(1), rect(0, 0, 120, 60)),
            (w(2), rect(0, 60, 60, 60)),
            (w(3), rect(60, 60, 60, 60)),
        ];
        assert_eq!(everything, mgr.layout_sorted(space, screen));

        // w2 isn't a member, so w3 takes the whole bottom row.
        let context = ActiveContext {
            key: c,
            members: [w(1), w(3)].into_iter().collect(),
        };
        _ = mgr.handle_event(SpaceExposed(space, screen.size, context));
        assert_eq!(
            vec![(w(1), rect(0, 0, 120, 60)), (w(3), rect(0, 60, 120, 60))],
            mgr.layout_sorted(space, screen),
        );

        _ = mgr.handle_event(SpaceExposed(space, screen.size, EVERYTHING));
        assert_eq!(everything, mgr.layout_sorted(space, screen));
    }

    /// L1, L2. The regression test for keeping layouts across switches.
    #[test]
    fn switching_contexts_away_and_back_gives_the_same_frames() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3), w(4)];
        let [x, y] = named_contexts(["X", "Y"]);
        let x_members = [w(1), w(2), w(3)];
        let y_members = [w(2), w(3), w(4)];

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        let everything = vec![
            (w(1), rect(0, 0, 30, 120)),
            (w(2), rect(30, 0, 30, 120)),
            (w(3), rect(60, 0, 30, 120)),
            (w(4), rect(90, 0, 30, 120)),
        ];
        assert_eq!(everything, mgr.layout_sorted(space, screen));

        switch(&mut mgr, space, screen.size, x, &x_members);
        _ = mgr.handle_event(WindowFocused(vec![space], w(1)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        let x_frames = vec![
            (w(1), rect(0, 0, 120, 60)),
            (w(2), rect(0, 60, 60, 60)),
            (w(3), rect(60, 60, 60, 60)),
        ];
        assert_eq!(x_frames, mgr.layout_sorted(space, screen));

        // Y starts from X's layout without w1, and w4 joins at the end.
        switch(&mut mgr, space, screen.size, y, &y_members);
        assert_eq!(
            vec![
                (w(2), rect(0, 0, 60, 60)),
                (w(3), rect(60, 0, 60, 60)),
                (w(4), rect(0, 60, 120, 60)),
            ],
            mgr.layout_sorted(space, screen),
        );
        _ = mgr.handle_event(WindowFocused(vec![space], w(4)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        let y_frames = vec![
            (w(2), rect(0, 0, 40, 120)),
            (w(3), rect(80, 0, 40, 120)),
            (w(4), rect(40, 0, 40, 120)),
        ];
        assert_eq!(y_frames, mgr.layout_sorted(space, screen));

        for _ in 0..2 {
            switch(&mut mgr, space, screen.size, x, &x_members);
            assert_eq!(x_frames, mgr.layout_sorted(space, screen));
            switch(&mut mgr, space, screen.size, y, &y_members);
            assert_eq!(y_frames, mgr.layout_sorted(space, screen));
            switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
            assert_eq!(everything, mgr.layout_sorted(space, screen));
        }
    }

    /// L1.
    #[test]
    fn a_command_under_a_context_changes_only_the_contexts_layout() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c] = named_contexts(["C"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        let everything = vec![
            (w(1), rect(0, 0, 40, 120)),
            (w(2), rect(40, 0, 40, 120)),
            (w(3), rect(80, 0, 40, 120)),
        ];
        assert_eq!(everything, mgr.layout_sorted(space, screen));

        switch(&mut mgr, space, screen.size, c, &all);
        assert_eq!(everything, mgr.layout_sorted(space, screen));
        _ = mgr.handle_event(WindowFocused(vec![space], w(3)));
        _ = mgr.handle_command(Some(space), &[space], Split(Orientation::Vertical));
        _ = mgr.handle_command(
            Some(space),
            &[space],
            Resize {
                direction: Direction::Down,
                percent: 25.0,
            },
        );
        let c_frames = vec![
            (w(1), rect(0, 0, 60, 120)),
            (w(2), rect(60, 90, 60, 30)),
            (w(3), rect(60, 0, 60, 90)),
        ];
        assert_eq!(c_frames, mgr.layout_sorted(space, screen));

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        assert_eq!(everything, mgr.layout_sorted(space, screen));
        switch(&mut mgr, space, screen.size, c, &all);
        assert_eq!(c_frames, mgr.layout_sorted(space, screen));
    }

    /// L2.
    #[test]
    fn next_and_prev_layout_do_nothing_under_a_context() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen1 = rect(0, 0, 120, 120);
        let screen2 = rect(0, 0, 1200, 1200);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c] = named_contexts(["C"]);

        // Give C two layouts, one per screen size.
        switch(&mut mgr, space, screen1.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen1.size, c, &all);
        _ = mgr.handle_event(WindowFocused(vec![space], w(1)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        switch(&mut mgr, space, screen2.size, c, &all);
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Down));
        switch(&mut mgr, space, screen1.size, c, &all);
        assert_eq!(2, mgr.context_layouts[&(space, c)].layouts().len());

        let frames = vec![
            (w(1), rect(0, 0, 120, 60)),
            (w(2), rect(0, 60, 60, 60)),
            (w(3), rect(60, 60, 60, 60)),
        ];
        assert_eq!(frames, mgr.layout_sorted(space, screen1));
        _ = mgr.handle_command(Some(space), &[space], NextLayout);
        assert_eq!(frames, mgr.layout_sorted(space, screen1));
        _ = mgr.handle_command(Some(space), &[space], PrevLayout);
        assert_eq!(frames, mgr.layout_sorted(space, screen1));
    }

    /// L2.
    #[test]
    fn a_context_keeps_a_layout_per_screen_size() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen1 = rect(0, 0, 120, 120);
        let screen2 = rect(0, 0, 1200, 1200);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c] = named_contexts(["C"]);

        switch(&mut mgr, space, screen1.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen1.size, c, &all);
        _ = mgr.handle_event(WindowFocused(vec![space], w(1)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        let size1_frames = vec![
            (w(1), rect(0, 0, 120, 60)),
            (w(2), rect(0, 60, 60, 60)),
            (w(3), rect(60, 60, 60, 60)),
        ];
        assert_eq!(size1_frames, mgr.layout_sorted(space, screen1));

        // A new size starts from the same layout, scaled.
        switch(&mut mgr, space, screen2.size, c, &all);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 1200, 600)),
                (w(2), rect(0, 600, 600, 600)),
                (w(3), rect(600, 600, 600, 600)),
            ],
            mgr.layout_sorted(space, screen2),
        );

        // Changing it there gives the new size its own layout.
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Down));
        let size2_frames = vec![
            (w(1), rect(0, 0, 400, 1200)),
            (w(2), rect(400, 0, 400, 1200)),
            (w(3), rect(800, 0, 400, 1200)),
        ];
        assert_eq!(size2_frames, mgr.layout_sorted(space, screen2));

        switch(&mut mgr, space, screen1.size, c, &all);
        assert_eq!(size1_frames, mgr.layout_sorted(space, screen1));
        switch(&mut mgr, space, screen2.size, c, &all);
        assert_eq!(size2_frames, mgr.layout_sorted(space, screen2));

        // Everything was never changed, at either size.
        switch(&mut mgr, space, screen1.size, ContextKey::Everything, &all);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(40, 0, 40, 120)),
                (w(3), rect(80, 0, 40, 120)),
            ],
            mgr.layout_sorted(space, screen1),
        );
        switch(&mut mgr, space, screen2.size, ContextKey::Everything, &all);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 400, 1200)),
                (w(2), rect(400, 0, 400, 1200)),
                (w(3), rect(800, 0, 400, 1200)),
            ],
            mgr.layout_sorted(space, screen2),
        );
    }

    /// L1.
    #[test]
    fn turning_off_scroll_layouts_converts_context_layouts() {
        let mut mgr = LayoutManager::new_for_test();
        mgr.set_config(&config_with_scroll(true, LayoutKind::Scroll));
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 900, 600);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen.size, c, &all);
        switch(&mut mgr, space, screen.size, d, &all);
        let kinds = |mgr: &LayoutManager| {
            [ContextKey::Everything, c, d]
                .map(|key| mgr.tree.layout_kind(mgr.mapping(space, key).unwrap().active_layout()))
        };
        assert_eq!([LayoutKind::Scroll; 3], kinds(&mgr));

        mgr.set_config(&config_with_scroll(false, LayoutKind::Tree));
        assert_eq!([LayoutKind::Tree; 3], kinds(&mgr));
    }

    /// L2.
    #[test]
    fn a_missing_context_mapping_falls_back_to_everything() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2)];
        let [c] = named_contexts(["C"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        mgr.active_contexts.insert(space, c);
        let everything = mgr.layout_mapping[&space].active_layout();
        assert_eq!(1, count_errors(|| assert_eq!(everything, mgr.layout(space))));
        assert_eq!(
            vec![(w(1), rect(0, 0, 60, 120)), (w(2), rect(60, 0, 60, 120))],
            mgr.layout_sorted(space, screen),
        );

        // Commands change the layout the Space shows.
        _ = mgr.handle_event(WindowFocused(vec![space], w(1)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        assert_eq!(
            vec![(w(1), rect(0, 0, 120, 60)), (w(2), rect(0, 60, 120, 60))],
            mgr.layout_sorted(space, screen),
        );
        assert!(mgr.context_layouts.is_empty());
    }

    /// L7, L9.
    #[test]
    fn floating_a_window_in_one_context_keeps_its_node_in_another() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        let side_by_side = vec![
            (w(1), rect(0, 0, 40, 120)),
            (w(2), rect(40, 0, 40, 120)),
            (w(3), rect(80, 0, 40, 120)),
        ];
        switch(&mut mgr, space, screen.size, d, &all);
        assert_eq!(side_by_side, mgr.layout_sorted(space, screen));
        switch(&mut mgr, space, screen.size, c, &all);
        _ = mgr.handle_event(WindowFocused(vec![space], w(1)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));

        _ = mgr.handle_event(WindowFocused(vec![space], w(2)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(
            vec![(w(1), rect(0, 0, 120, 60)), (w(3), rect(0, 60, 120, 60))],
            mgr.layout_sorted(space, screen),
        );
        let d_layout = mgr.context_layouts[&(space, d)].active_layout();
        assert!(mgr.tree.window_node(d_layout, w(2)).is_some());
        let everything_layout = mgr.layout_mapping[&space].active_layout();
        assert!(mgr.tree.window_node(everything_layout, w(2)).is_some());

        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 120, 40)),
                (w(2), rect(0, 80, 120, 40)),
                (w(3), rect(0, 40, 120, 40)),
            ],
            mgr.layout_sorted(space, screen),
        );

        switch(&mut mgr, space, screen.size, d, &all);
        assert_eq!(side_by_side, mgr.layout_sorted(space, screen));
        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        assert_eq!(side_by_side, mgr.layout_sorted(space, screen));
    }

    /// L7, L9.
    #[test]
    fn a_window_floated_in_one_context_floats_in_the_others() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen.size, d, &all);
        switch(&mut mgr, space, screen.size, c, &all);
        _ = mgr.handle_event(WindowFocused(vec![space], w(2)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);

        // D's layout drops w2 when D shows, because w2 still floats.
        switch(&mut mgr, space, screen.size, d, &all);
        assert_eq!(
            vec![(w(1), rect(0, 0, 60, 120)), (w(3), rect(60, 0, 60, 120))],
            mgr.layout_sorted(space, screen),
        );
        assert!(mgr.floating_windows_in_space(space).contains(&w(2)));

        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(80, 0, 40, 120)),
                (w(3), rect(40, 0, 40, 120)),
            ],
            mgr.layout_sorted(space, screen),
        );
        switch(&mut mgr, space, screen.size, c, &all);
        assert!(!mgr.floating_windows_in_space(space).contains(&w(2)));
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(80, 0, 40, 120)),
                (w(3), rect(40, 0, 40, 120)),
            ],
            mgr.layout_sorted(space, screen),
        );
    }

    /// L9.
    #[test]
    fn changing_the_layout_kind_in_one_context_keeps_nodes_in_another() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        mgr.set_config(&config_with_scroll(true, LayoutKind::Tree));
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen.size, d, &all);
        _ = mgr.handle_event(WindowFocused(vec![space], w(1)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        let d_frames = vec![
            (w(1), rect(0, 0, 120, 60)),
            (w(2), rect(0, 60, 60, 60)),
            (w(3), rect(60, 60, 60, 60)),
        ];
        assert_eq!(d_frames, mgr.layout_sorted(space, screen));

        switch(&mut mgr, space, screen.size, c, &all);
        _ = mgr.handle_command(Some(space), &[space], ChangeLayoutKind);
        assert_eq!(LayoutKind::Scroll, mgr.active_layout_kind(space));
        let c_layout = mgr.context_layouts[&(space, c)].active_layout();
        for wid in all {
            assert!(mgr.tree.window_node(c_layout, wid).is_some());
        }

        switch(&mut mgr, space, screen.size, d, &all);
        assert_eq!(d_frames, mgr.layout_sorted(space, screen));
        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(40, 0, 40, 120)),
                (w(3), rect(80, 0, 40, 120)),
            ],
            mgr.layout_sorted(space, screen),
        );
    }

    /// A floating window gets no tile at another screen size, even before the
    /// next update of the windows on screen.
    #[test]
    fn floating_a_window_removes_it_from_the_layouts_of_every_size() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen1 = rect(0, 0, 120, 120);
        let screen2 = rect(0, 0, 1200, 1200);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];

        switch(&mut mgr, space, screen1.size, ContextKey::Everything, &all);
        _ = mgr.handle_event(WindowFocused(vec![space], w(1)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        switch(&mut mgr, space, screen2.size, ContextKey::Everything, &all);
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Down));
        switch(&mut mgr, space, screen1.size, ContextKey::Everything, &all);

        _ = mgr.handle_event(WindowFocused(vec![space], w(2)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        _ = mgr.handle_event(SpaceExposed(space, screen2.size, EVERYTHING));
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 600, 1200)),
                (w(3), rect(600, 0, 600, 1200)),
            ],
            mgr.layout_sorted(space, screen2),
        );
    }

    /// A window floated in one context keeps its node in another until that
    /// context's windows are sent again. Unfloating it there reuses the node.
    #[test]
    fn unfloating_a_window_keeps_one_node_per_layout() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen.size, d, &all);
        switch(&mut mgr, space, screen.size, c, &all);
        _ = mgr.handle_event(WindowFocused(vec![space], w(2)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);

        let d_context = ActiveContext {
            key: d,
            members: all.into_iter().collect(),
        };
        _ = mgr.handle_event(SpaceExposed(space, screen.size, d_context));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(40, 0, 40, 120)),
                (w(3), rect(80, 0, 40, 120)),
            ],
            mgr.layout_sorted(space, screen),
        );
    }

    fn context_id(key: ContextKey) -> ContextId {
        match key {
            ContextKey::Named(id) => id,
            _ => panic!("{key:?} has no id"),
        }
    }

    /// R6, L2.
    #[test]
    fn deleting_a_context_removes_all_its_layouts() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let screen1 = rect(0, 0, 120, 120);
        let screen2 = rect(0, 0, 1200, 1200);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);

        // D has a layout on space 1. C has two on space 1 and one on space 2.
        switch(&mut mgr, space1, screen1.size, ContextKey::Everything, &all);
        switch(&mut mgr, space2, screen1.size, ContextKey::Everything, &[]);
        switch(&mut mgr, space1, screen1.size, d, &all);
        _ = mgr.handle_event(WindowFocused(vec![space1], w(1)));
        _ = mgr.handle_command(Some(space1), &[space1], MoveNode(Direction::Up));
        let d_frames = mgr.layout_sorted(space1, screen1);
        switch(&mut mgr, space2, screen1.size, c, &[]);
        switch(&mut mgr, space1, screen1.size, c, &all);
        _ = mgr.handle_command(Some(space1), &[space1], MoveNode(Direction::Down));
        switch(&mut mgr, space1, screen2.size, c, &all);
        _ = mgr.handle_command(Some(space1), &[space1], MoveNode(Direction::Up));
        switch(&mut mgr, space1, screen1.size, c, &all);
        assert_eq!(2, mgr.context_layouts[&(space1, c)].layouts().len());
        let layouts_before = mgr.tree.layouts().len();

        mgr.remove_context_layouts(context_id(c));
        let mut keys = mgr.context_layouts.keys().copied().collect::<Vec<_>>();
        keys.sort_by_key(|&(space, _)| space);
        assert_eq!(vec![(space1, d)], keys);
        assert_eq!(layouts_before - 3, mgr.tree.layouts().len());

        // C was active on space 1, so the space shows Everything's layout.
        let everything = mgr.layout_mapping[&space1].active_layout();
        assert_eq!(0, count_errors(|| assert_eq!(everything, mgr.layout(space1))));

        switch(&mut mgr, space1, screen1.size, d, &all);
        assert_eq!(d_frames, mgr.layout_sorted(space1, screen1));
        switch(&mut mgr, space1, screen1.size, ContextKey::Everything, &all);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(40, 0, 40, 120)),
                (w(3), rect(80, 0, 40, 120)),
            ],
            mgr.layout_sorted(space1, screen1),
        );
    }

    /// L2.
    #[test]
    fn loading_drops_the_layouts_of_unknown_contexts() {
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen.size, ContextKey::Unsorted, &all);
        switch(&mut mgr, space, screen.size, c, &all);
        switch(&mut mgr, space, screen.size, d, &all);

        let mut restored: LayoutManager = ron::from_str(&mgr.serialize_to_string()).unwrap();
        assert_eq!(4, restored.tree.layouts().len());
        restored.retain_context_layouts(|id| id == context_id(c));
        let mut keys = restored.context_layouts.keys().copied().collect::<Vec<_>>();
        keys.sort_by_key(|&(_, key)| key != ContextKey::Unsorted);
        assert_eq!(vec![(space, ContextKey::Unsorted), (space, c)], keys);
        assert_eq!(3, restored.tree.layouts().len());
    }

    /// Context layouts are saved in `layout.ron`, but the active context is not.
    #[test]
    fn context_layouts_survive_a_save_and_restore() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c] = named_contexts(["C"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen.size, ContextKey::Unsorted, &[w(3)]);
        // C starts from Unsorted's layout, so w3 comes first.
        switch(&mut mgr, space, screen.size, c, &all);
        _ = mgr.handle_event(WindowFocused(vec![space], w(1)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        let c_frames = vec![
            (w(1), rect(0, 0, 120, 60)),
            (w(2), rect(60, 60, 60, 60)),
            (w(3), rect(0, 60, 60, 60)),
        ];
        assert_eq!(c_frames, mgr.layout_sorted(space, screen));

        let mut restored: LayoutManager = ron::from_str(&mgr.serialize_to_string()).unwrap();
        let mut keys = restored.context_layouts.keys().copied().collect::<Vec<_>>();
        keys.sort_by_key(|&(_, key)| key != ContextKey::Unsorted);
        assert_eq!(vec![(space, ContextKey::Unsorted), (space, c)], keys);
        assert!(restored.active_contexts.is_empty());
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(40, 0, 40, 120)),
                (w(3), rect(80, 0, 40, 120)),
            ],
            restored.layout_sorted(space, screen),
        );

        switch(&mut restored, space, screen.size, c, &all);
        assert_eq!(c_frames, restored.layout_sorted(space, screen));
        switch(&mut restored, space, screen.size, ContextKey::Unsorted, &[w(3)]);
        assert_eq!(
            vec![(w(3), rect(0, 0, 120, 120))],
            restored.layout_sorted(space, screen)
        );
    }

    /// R9. A window away on another display has no gap in its old layout,
    /// and a layout save while it is away keeps its tile for the return.
    #[test]
    fn a_moved_window_keeps_its_tile_through_a_layout_save() {
        let mut mgr = LayoutManager::new_for_test();
        let left = SpaceId::new(1);
        let right = SpaceId::new(2);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);

        switch(&mut mgr, right, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, right, screen.size, c, &all);
        switch(&mut mgr, left, screen.size, d, &[]);
        let before = mgr.layout_sorted(right, screen);

        mgr.move_window_to_space(w(2), right, left);
        switch(&mut mgr, right, screen.size, c, &[w(1), w(3)]);
        switch(&mut mgr, left, screen.size, d, &[w(2)]);
        assert_eq!(2, mgr.layout_sorted(right, screen).len());

        let mut restored: LayoutManager = ron::from_str(&mgr.serialize_to_string()).unwrap();
        switch(&mut restored, right, screen.size, c, &[w(1), w(3)]);
        switch(&mut restored, left, screen.size, d, &[w(2)]);
        restored.move_window_to_space(w(2), left, right);
        switch(&mut restored, right, screen.size, c, &all);

        assert_eq!(before, restored.layout_sorted(right, screen));
    }

    /// How `layout.ron` stores the layouts of a named context and of
    /// Unsorted. Older files must keep loading, so this must not change.
    #[test]
    fn context_layouts_keep_their_saved_format() {
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2)];
        let [c] = named_contexts(["C"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen.size, ContextKey::Unsorted, &[w(2)]);
        switch(&mut mgr, space, screen.size, c, &[w(1)]);

        let saved = mgr.serialize_to_string();
        let start = saved.find("context_layouts:").unwrap();
        let end = start + saved[start..].find(",floating_windows:").unwrap();
        let expected = "context_layouts:{\
            ((1),Unsorted):(\
                active_size:(width:120,height:120),active_layout:(idx:2,version:1),\
                active_save_state:Unretained,memory:{},layouts:{(idx:2,version:1):1}\
            ),\
            ((1),Named(1)):(\
                active_size:(width:120,height:120),active_layout:(idx:3,version:1),\
                active_save_state:Unretained,memory:{},layouts:{(idx:3,version:1):1}\
            )\
        }";
        assert_eq!(expected, &saved[start..end]);

        let restored: LayoutManager = ron::from_str(&saved).unwrap();
        assert_eq!(context_keys(&mgr), context_keys(&restored));
    }

    /// Shows `key` on the Space without sending its windows again.
    fn show(mgr: &mut LayoutManager, space: SpaceId, size: CGSize, key: ContextKey) {
        let context = ActiveContext { key, members: BTreeSet::new() };
        _ = mgr.handle_event(LayoutEvent::SpaceExposed(space, size, context));
    }

    /// The frames the Space shows after `show`.
    fn shown_frames(
        mgr: &mut LayoutManager,
        space: SpaceId,
        screen: CGRect,
        key: ContextKey,
    ) -> Vec<(WindowId, CGRect)> {
        show(mgr, space, screen.size, key);
        mgr.layout_sorted(space, screen)
    }

    /// The frames the Space shows after `switch`.
    fn switched_frames(
        mgr: &mut LayoutManager,
        space: SpaceId,
        screen: CGRect,
        key: ContextKey,
        showing: &[WindowId],
    ) -> Vec<(WindowId, CGRect)> {
        switch(mgr, space, screen.size, key, showing);
        mgr.layout_sorted(space, screen)
    }

    /// Focuses the window and moves it.
    fn move_window(mgr: &mut LayoutManager, space: SpaceId, wid: WindowId, direction: Direction) {
        _ = mgr.handle_event(LayoutEvent::WindowFocused(vec![space], wid));
        _ = mgr.handle_command(Some(space), &[space], LayoutCommand::MoveNode(direction));
    }

    const NO_FRAMES: Vec<(WindowId, CGRect)> = Vec::new();

    fn context_keys(mgr: &LayoutManager) -> HashSet<(SpaceId, ContextKey)> {
        mgr.context_layouts.keys().copied().collect()
    }

    fn contexts_on() -> Arc<Config> {
        let mut config = Config::default();
        config.settings.experimental.contexts.enable = true;
        Arc::new(config)
    }

    /// Shows each of `keys` on the Space at each size of `screens`, and moves
    /// a window in each, so that every pair gets a layout of its own.
    fn give_each_its_own_layout(
        mgr: &mut LayoutManager,
        space: SpaceId,
        screens: &[CGRect],
        keys: &[ContextKey],
        windows: &[WindowId],
    ) {
        for &key in keys {
            for screen in screens {
                switch(mgr, space, screen.size, key, windows);
                move_window(mgr, space, windows[1], Direction::Up);
            }
        }
    }

    /// L10.
    #[test]
    fn a_window_that_leaves_a_space_leaves_every_layout_there() {
        let mut mgr = LayoutManager::new_for_test();
        mgr.set_config(&contexts_on());
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let small = rect(0, 0, 120, 120);
        let wide = rect(0, 0, 240, 120);
        let w = |idx| WindowId::new(1, idx);
        let [c, d] = named_contexts(["C", "D"]);
        let keys = [ContextKey::Everything, c, d];
        switch(&mut mgr, space2, small.size, ContextKey::Everything, &[]);
        give_each_its_own_layout(&mut mgr, space1, &[small, wide], &keys, &[w(1), w(2)]);
        assert_eq!(2, mgr.context_layouts[&(space1, c)].layouts().len());

        _ = mgr.handle_event(LayoutEvent::WindowSpaceChanged {
            wid: w(1),
            added: Some(space2),
            removed: Some(space1),
            info: win_info(),
            contexts_in_use: true,
        });

        for key in keys {
            for screen in [small, wide] {
                assert_eq!(
                    vec![(w(2), screen)],
                    shown_frames(&mut mgr, space1, screen, key),
                    "{key:?} at {screen:?}"
                );
            }
        }
        assert_eq!(
            vec![(w(1), small)],
            shown_frames(&mut mgr, space2, small, ContextKey::Everything)
        );
    }

    /// R28. Without contexts, a window that leaves a Space leaves only the
    /// layout the Space shows.
    #[test]
    fn without_contexts_a_window_that_leaves_a_space_leaves_only_the_shown_layout() {
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let small = rect(0, 0, 120, 120);
        let wide = rect(0, 0, 240, 120);
        let w = |idx| WindowId::new(1, idx);
        let everything = ContextKey::Everything;
        switch(&mut mgr, space2, small.size, everything, &[]);
        give_each_its_own_layout(&mut mgr, space1, &[small, wide], &[everything], &[w(1), w(2)]);

        _ = mgr.handle_event(LayoutEvent::WindowSpaceChanged {
            wid: w(1),
            added: Some(space2),
            removed: Some(space1),
            info: win_info(),
            contexts_in_use: false,
        });

        assert_eq!(
            vec![(w(2), wide)],
            shown_frames(&mut mgr, space1, wide, everything)
        );
        assert_eq!(
            vec![w(1), w(2)],
            shown_frames(&mut mgr, space1, small, everything)
                .into_iter()
                .map(|(wid, _)| wid)
                .collect::<Vec<_>>()
        );
    }

    /// R2, L1, L2. Each context keeps its own layout on each Space and screen
    /// size, and Everything's layouts don't change.
    #[test]
    fn contexts_on_two_spaces_keep_a_layout_per_screen_size() {
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let small = rect(0, 0, 120, 120);
        let wide = rect(0, 0, 240, 120);
        let w = |idx| WindowId::new(1, idx);
        let on1 = [w(1), w(2), w(3)];
        let on2 = [w(4), w(5), w(6)];
        let d_on1 = [w(2), w(3)];
        let d_on2 = [w(4), w(5)];
        let [c, d] = named_contexts(["C", "D"]);
        let everything = ContextKey::Everything;

        switch(&mut mgr, space1, small.size, everything, &on1);
        switch(&mut mgr, space2, small.size, everything, &on2);
        switch(&mut mgr, space1, small.size, c, &on1);
        move_window(&mut mgr, space1, w(1), Direction::Up);
        // D starts from C's layout without w1.
        switch(&mut mgr, space1, small.size, d, &d_on1);
        move_window(&mut mgr, space1, w(3), Direction::Up);
        switch(&mut mgr, space1, wide.size, c, &on1);
        move_window(&mut mgr, space1, w(3), Direction::Up);
        switch(&mut mgr, space2, small.size, c, &on2);
        move_window(&mut mgr, space2, w(6), Direction::Up);
        // D starts from C's layout without w6.
        switch(&mut mgr, space2, wide.size, d, &d_on2);
        move_window(&mut mgr, space2, w(5), Direction::Up);

        assert_eq!(
            [(space1, c), (space1, d), (space2, c), (space2, d)]
                .into_iter()
                .collect::<HashSet<_>>(),
            context_keys(&mgr),
        );
        assert_eq!(2, mgr.context_layouts[&(space1, c)].layouts().len());

        for _ in 0..2 {
            assert_eq!(
                vec![
                    (w(1), rect(0, 0, 40, 120)),
                    (w(2), rect(40, 0, 40, 120)),
                    (w(3), rect(80, 0, 40, 120)),
                ],
                switched_frames(&mut mgr, space1, small, everything, &on1),
            );
            assert_eq!(
                vec![
                    (w(1), rect(0, 0, 80, 120)),
                    (w(2), rect(80, 0, 80, 120)),
                    (w(3), rect(160, 0, 80, 120)),
                ],
                switched_frames(&mut mgr, space1, wide, everything, &on1),
            );
            assert_eq!(
                vec![
                    (w(1), rect(0, 0, 120, 60)),
                    (w(2), rect(0, 60, 60, 60)),
                    (w(3), rect(60, 60, 60, 60)),
                ],
                switched_frames(&mut mgr, space1, small, c, &on1),
            );
            assert_eq!(
                vec![
                    (w(1), rect(0, 0, 240, 40)),
                    (w(2), rect(0, 80, 240, 40)),
                    (w(3), rect(0, 40, 240, 40)),
                ],
                switched_frames(&mut mgr, space1, wide, c, &on1),
            );
            assert_eq!(
                vec![(w(2), rect(0, 60, 120, 60)), (w(3), rect(0, 0, 120, 60))],
                switched_frames(&mut mgr, space1, small, d, &d_on1),
            );
            assert_eq!(
                vec![(w(2), rect(0, 60, 240, 60)), (w(3), rect(0, 0, 240, 60))],
                switched_frames(&mut mgr, space1, wide, d, &d_on1),
            );
            assert_eq!(
                vec![
                    (w(4), rect(0, 0, 40, 120)),
                    (w(5), rect(40, 0, 40, 120)),
                    (w(6), rect(80, 0, 40, 120)),
                ],
                switched_frames(&mut mgr, space2, small, everything, &on2),
            );
            assert_eq!(
                vec![
                    (w(4), rect(0, 0, 80, 120)),
                    (w(5), rect(80, 0, 80, 120)),
                    (w(6), rect(160, 0, 80, 120)),
                ],
                switched_frames(&mut mgr, space2, wide, everything, &on2),
            );
            assert_eq!(
                vec![
                    (w(4), rect(0, 60, 60, 60)),
                    (w(5), rect(60, 60, 60, 60)),
                    (w(6), rect(0, 0, 120, 60)),
                ],
                switched_frames(&mut mgr, space2, small, c, &on2),
            );
            assert_eq!(
                vec![
                    (w(4), rect(0, 60, 120, 60)),
                    (w(5), rect(120, 60, 120, 60)),
                    (w(6), rect(0, 0, 240, 60)),
                ],
                switched_frames(&mut mgr, space2, wide, c, &on2),
            );
            assert_eq!(
                vec![(w(4), rect(0, 60, 120, 60)), (w(5), rect(0, 0, 120, 60))],
                switched_frames(&mut mgr, space2, small, d, &d_on2),
            );
            assert_eq!(
                vec![(w(4), rect(0, 60, 240, 60)), (w(5), rect(0, 0, 240, 60))],
                switched_frames(&mut mgr, space2, wide, d, &d_on2),
            );
        }
    }

    /// L1. A split, a move into the nested container, and a resize inside it
    /// change only C's layout.
    #[test]
    fn a_nested_split_under_a_context_changes_only_that_contexts_layout() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let on1 = [w(1), w(2), w(3), w(4)];
        let on2 = [w(5), w(6)];
        let [c, d] = named_contexts(["C", "D"]);
        let everything = ContextKey::Everything;
        let visible = [space1, space2];

        switch(&mut mgr, space1, screen.size, everything, &on1);
        switch(&mut mgr, space2, screen.size, everything, &on2);
        switch(&mut mgr, space1, screen.size, d, &on1);
        switch(&mut mgr, space1, screen.size, c, &on1);

        // C becomes [w1 / [w2 | [w3 / w4]]], with w3 taking 3/4 of its column.
        _ = mgr.handle_event(WindowFocused(vec![space1], w(1)));
        _ = mgr.handle_command(Some(space1), &visible, MoveNode(Direction::Up));
        _ = mgr.handle_event(WindowFocused(vec![space1], w(3)));
        _ = mgr.handle_command(Some(space1), &visible, Split(Orientation::Vertical));
        _ = mgr.handle_event(WindowFocused(vec![space1], w(4)));
        _ = mgr.handle_command(Some(space1), &visible, MoveNode(Direction::Left));
        _ = mgr.handle_event(WindowFocused(vec![space1], w(3)));
        _ = mgr.handle_command(
            Some(space1),
            &visible,
            Resize {
                direction: Direction::Down,
                percent: 12.5,
            },
        );
        let c_frames = vec![
            (w(1), rect(0, 0, 120, 60)),
            (w(2), rect(0, 60, 60, 60)),
            (w(3), rect(60, 60, 60, 45)),
            (w(4), rect(60, 105, 60, 15)),
        ];
        assert_eq!(c_frames, mgr.layout_sorted(space1, screen));

        let side_by_side = vec![
            (w(1), rect(0, 0, 30, 120)),
            (w(2), rect(30, 0, 30, 120)),
            (w(3), rect(60, 0, 30, 120)),
            (w(4), rect(90, 0, 30, 120)),
        ];
        let space2_frames = vec![(w(5), rect(0, 0, 60, 120)), (w(6), rect(60, 0, 60, 120))];
        assert_eq!(space2_frames, mgr.layout_sorted(space2, screen));
        assert_eq!(side_by_side, shown_frames(&mut mgr, space1, screen, d));
        assert_eq!(side_by_side, shown_frames(&mut mgr, space1, screen, everything));

        for _ in 0..2 {
            assert_eq!(side_by_side, switched_frames(&mut mgr, space1, screen, d, &on1));
            assert_eq!(c_frames, switched_frames(&mut mgr, space1, screen, c, &on1));
            assert_eq!(
                side_by_side,
                switched_frames(&mut mgr, space1, screen, everything, &on1)
            );
            assert_eq!(
                space2_frames,
                switched_frames(&mut mgr, space2, screen, everything, &on2)
            );
        }
    }

    /// R2, L1. A context keeps a layout per Space, so a command under C on
    /// one Space leaves C's layout on the other Space alone.
    #[test]
    fn a_command_under_a_context_changes_its_layout_only_on_that_space() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let on1 = [w(1), w(2), w(3)];
        let on2 = [w(4), w(5), w(6)];
        let [c] = named_contexts(["C"]);
        let everything = ContextKey::Everything;
        let everything1 = vec![
            (w(1), rect(0, 0, 40, 120)),
            (w(2), rect(40, 0, 40, 120)),
            (w(3), rect(80, 0, 40, 120)),
        ];
        let everything2 = vec![
            (w(4), rect(0, 0, 40, 120)),
            (w(5), rect(40, 0, 40, 120)),
            (w(6), rect(80, 0, 40, 120)),
        ];

        switch(&mut mgr, space1, screen.size, everything, &on1);
        switch(&mut mgr, space2, screen.size, everything, &on2);
        switch(&mut mgr, space1, screen.size, c, &on1);
        switch(&mut mgr, space2, screen.size, c, &on2);
        assert_eq!(everything1, mgr.layout_sorted(space1, screen));
        assert_eq!(everything2, mgr.layout_sorted(space2, screen));

        _ = mgr.handle_event(WindowFocused(vec![space1], w(3)));
        _ = mgr.handle_command(Some(space1), &[space1], Split(Orientation::Vertical));
        _ = mgr.handle_command(
            Some(space1),
            &[space1],
            Resize {
                direction: Direction::Down,
                percent: 25.0,
            },
        );
        let c1_frames = vec![
            (w(1), rect(0, 0, 60, 120)),
            (w(2), rect(60, 90, 60, 30)),
            (w(3), rect(60, 0, 60, 90)),
        ];
        assert_eq!(c1_frames, mgr.layout_sorted(space1, screen));
        assert_eq!(everything2, mgr.layout_sorted(space2, screen));

        move_window(&mut mgr, space2, w(4), Direction::Up);
        let c2_frames = vec![
            (w(4), rect(0, 0, 120, 60)),
            (w(5), rect(0, 60, 60, 60)),
            (w(6), rect(60, 60, 60, 60)),
        ];
        assert_eq!(c2_frames, mgr.layout_sorted(space2, screen));
        assert_eq!(c1_frames, mgr.layout_sorted(space1, screen));

        for _ in 0..2 {
            assert_eq!(c2_frames, switched_frames(&mut mgr, space2, screen, c, &on2));
            assert_eq!(c1_frames, switched_frames(&mut mgr, space1, screen, c, &on1));
            assert_eq!(
                everything1,
                switched_frames(&mut mgr, space1, screen, everything, &on1)
            );
            assert_eq!(
                everything2,
                switched_frames(&mut mgr, space2, screen, everything, &on2)
            );
        }
    }

    /// L7, L9. Floating a window that is in C and D takes it out of C's
    /// layouts at every size. D keeps its place, and unfloating the window in
    /// C doesn't move it in D.
    #[test]
    fn floating_and_unfloating_a_shared_window_keeps_its_place_in_the_other_context() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let small = rect(0, 0, 120, 120);
        let wide = rect(0, 0, 240, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);
        let everything = ContextKey::Everything;
        let side_by_side = vec![
            (w(1), rect(0, 0, 40, 120)),
            (w(2), rect(40, 0, 40, 120)),
            (w(3), rect(80, 0, 40, 120)),
        ];

        switch(&mut mgr, space, small.size, everything, &all);
        switch(&mut mgr, space, small.size, c, &all);
        switch(&mut mgr, space, small.size, d, &all);
        move_window(&mut mgr, space, w(2), Direction::Up);
        let d_frames = vec![
            (w(1), rect(0, 60, 60, 60)),
            (w(2), rect(0, 0, 120, 60)),
            (w(3), rect(60, 60, 60, 60)),
        ];
        assert_eq!(d_frames, mgr.layout_sorted(space, small));
        switch(&mut mgr, space, small.size, c, &all);
        move_window(&mut mgr, space, w(1), Direction::Up);
        switch(&mut mgr, space, wide.size, c, &all);
        move_window(&mut mgr, space, w(3), Direction::Up);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 240, 40)),
                (w(2), rect(0, 80, 240, 40)),
                (w(3), rect(0, 40, 240, 40)),
            ],
            mgr.layout_sorted(space, wide),
        );
        switch(&mut mgr, space, small.size, c, &all);

        _ = mgr.handle_event(WindowFocused(vec![space], w(2)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(
            vec![(w(1), rect(0, 0, 120, 60)), (w(3), rect(0, 60, 120, 60))],
            mgr.layout_sorted(space, small),
        );
        // C's layout for the other size lost w2 too.
        assert_eq!(
            vec![(w(1), rect(0, 0, 240, 60)), (w(3), rect(0, 60, 240, 60))],
            shown_frames(&mut mgr, space, wide, c),
        );
        assert_eq!(d_frames, shown_frames(&mut mgr, space, small, d));
        assert_eq!(side_by_side, shown_frames(&mut mgr, space, small, everything));

        // w2 still has focus. Unfloating it in C puts it after C's selection.
        show(&mut mgr, space, small.size, c);
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        let c_small = vec![
            (w(1), rect(0, 0, 120, 40)),
            (w(2), rect(0, 80, 120, 40)),
            (w(3), rect(0, 40, 120, 40)),
        ];
        assert_eq!(c_small, mgr.layout_sorted(space, small));
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 240, 40)),
                (w(2), rect(0, 80, 240, 40)),
                (w(3), rect(0, 40, 240, 40)),
            ],
            switched_frames(&mut mgr, space, wide, c, &all),
        );
        assert_eq!(d_frames, switched_frames(&mut mgr, space, small, d, &all));
        assert_eq!(
            side_by_side,
            switched_frames(&mut mgr, space, small, everything, &all)
        );
        assert_eq!(c_small, switched_frames(&mut mgr, space, small, c, &all));
    }

    /// L9. Floating a window under C takes it out of C's layouts on every
    /// Space and at every size, before the windows are sent again. D and
    /// Everything keep their nodes for it.
    #[test]
    fn floating_under_a_context_removes_the_window_from_its_layouts_on_every_space() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let small = rect(0, 0, 120, 120);
        let wide = rect(0, 0, 240, 120);
        let w = |idx| WindowId::new(1, idx);
        let on1 = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);
        let everything = ContextKey::Everything;
        let side_by_side = vec![
            (w(1), rect(0, 0, 40, 120)),
            (w(2), rect(40, 0, 40, 120)),
            (w(3), rect(80, 0, 40, 120)),
        ];

        // w2 starts on space 2, where C's layout gets it. It then moves to
        // space 1, and only the layout space 2 shows loses it.
        switch(&mut mgr, space2, small.size, everything, &[w(2), w(4)]);
        switch(&mut mgr, space2, small.size, c, &[w(2), w(4)]);
        switch(&mut mgr, space2, small.size, everything, &[w(4)]);
        assert_eq!(
            vec![(w(2), rect(0, 0, 60, 120)), (w(4), rect(60, 0, 60, 120))],
            shown_frames(&mut mgr, space2, small, c),
        );
        show(&mut mgr, space2, small.size, everything);

        switch(&mut mgr, space1, small.size, everything, &on1);
        switch(&mut mgr, space1, small.size, d, &on1);
        switch(&mut mgr, space1, small.size, everything, &on1);
        switch(&mut mgr, space1, small.size, c, &on1);
        move_window(&mut mgr, space1, w(1), Direction::Up);
        switch(&mut mgr, space1, wide.size, c, &on1);
        move_window(&mut mgr, space1, w(3), Direction::Up);
        switch(&mut mgr, space1, small.size, c, &on1);
        assert_eq!(2, mgr.context_layouts[&(space1, c)].layouts().len());

        _ = mgr.handle_event(WindowFocused(vec![space1], w(2)));
        _ = mgr.handle_command(Some(space1), &[space1], ToggleWindowFloating);
        assert_eq!(
            vec![(w(1), rect(0, 0, 120, 60)), (w(3), rect(0, 60, 120, 60))],
            mgr.layout_sorted(space1, small),
        );
        assert_eq!(
            vec![(w(1), rect(0, 0, 240, 60)), (w(3), rect(0, 60, 240, 60))],
            shown_frames(&mut mgr, space1, wide, c),
        );
        assert_eq!(
            vec![(w(4), rect(0, 0, 120, 120))],
            shown_frames(&mut mgr, space2, small, c),
        );
        assert_eq!(side_by_side, shown_frames(&mut mgr, space1, small, d));
        assert_eq!(side_by_side, shown_frames(&mut mgr, space1, small, everything));
    }

    /// L9. Floating and unfloating a window under Everything leaves its place
    /// in a context's layout alone.
    #[test]
    fn floating_under_everything_keeps_a_windows_place_in_a_context() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c] = named_contexts(["C"]);
        let everything = ContextKey::Everything;

        switch(&mut mgr, space, screen.size, everything, &all);
        switch(&mut mgr, space, screen.size, c, &all);
        move_window(&mut mgr, space, w(1), Direction::Up);
        switch(&mut mgr, space, screen.size, everything, &all);

        _ = mgr.handle_event(WindowFocused(vec![space], w(2)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(
            vec![(w(1), rect(0, 0, 60, 120)), (w(3), rect(60, 0, 60, 120))],
            mgr.layout_sorted(space, screen),
        );
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(80, 0, 40, 120)),
                (w(3), rect(40, 0, 40, 120)),
            ],
            mgr.layout_sorted(space, screen),
        );

        assert_eq!(
            vec![
                (w(1), rect(0, 0, 120, 60)),
                (w(2), rect(0, 60, 60, 60)),
                (w(3), rect(60, 60, 60, 60)),
            ],
            switched_frames(&mut mgr, space, screen, c, &all),
        );
    }

    /// L9. With no context active, unfloating a window that got a tile while
    /// it floated reuses that tile instead of adding a second one.
    #[test]
    fn unfloating_under_everything_reuses_a_tile_the_window_got_while_floating() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        _ = mgr.handle_event(WindowFocused(vec![space], w(2)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        _ = mgr.handle_event(WindowAdded(space, w(2), win_info()));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert!(mgr.floating_windows_in_space(space).is_empty());
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(80, 0, 40, 120)),
                (w(3), rect(40, 0, 40, 120)),
            ],
            mgr.layout_sorted(space, screen),
        );
    }

    /// L7. A window has one floating frame, whichever context it floats in.
    #[test]
    fn native_tab_selection_keeps_the_floating_frame_and_active_window() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        switch(
            &mut mgr,
            space,
            screen.size,
            ContextKey::Everything,
            &[w(1), w(2), w(3)],
        );
        _ = mgr.handle_event(WindowFocused(vec![space], w(1)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        let moved = rect(10, 20, 50, 40);
        _ = mgr.handle_event(WindowFrameChanged { wid: w(1), frame: moved });
        _ = mgr.handle_event(NativeTabSelected {
            previous: w(1),
            selected: w(2),
            members: vec![w(1), w(2)],
        });
        assert_eq!(
            mgr.floating_windows_in_space(space),
            [w(2)].into_iter().collect()
        );
        assert_eq!(mgr.floating_restore_frame(w(2)), Some(moved));
        assert_eq!(mgr.focused_window, Some(w(2)));
        assert_eq!(mgr.layout_sorted(space, screen), vec![(w(3), screen)]);
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        let response = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(response.frame_overrides, vec![(w(2), moved)]);
        assert_eq!(mgr.layout_sorted(space, screen), vec![(w(3), screen)]);
    }

    /// L7. A window has one floating frame, whichever context it floats in.
    #[test]
    fn a_shared_window_floats_at_the_same_frame_in_every_context() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen.size, c, &all);
        switch(&mut mgr, space, screen.size, d, &all);
        switch(&mut mgr, space, screen.size, c, &all);

        _ = mgr.handle_event(WindowFocused(vec![space], w(2)));
        let response = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(vec![(w(2), CGRect::ZERO)], response.frame_overrides);
        let moved = rect(10, 20, 50, 40);
        _ = mgr.handle_event(WindowFrameChanged { wid: w(2), frame: moved });
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);

        switch(&mut mgr, space, screen.size, d, &all);
        _ = mgr.handle_event(WindowFocused(vec![space], w(2)));
        let response = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(vec![(w(2), moved)], response.frame_overrides);
        assert_eq!(Some(moved), mgr.floating_restore_frame(w(2)));
    }

    /// L7. A size lock belongs to the window, so a lock set under C also holds
    /// in D and in Everything.
    #[test]
    fn a_size_lock_set_in_one_context_holds_in_the_others() {
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen.size, d, &all);
        switch(&mut mgr, space, screen.size, c, &all);
        _ = mgr.handle_event(LayoutEvent::WindowFocused(vec![space], w(1)));
        let feedback = set_size_share(&mut mgr, space, SizeShare::Fraction(0.5));
        assert_eq!(Some(SizeShareOutcome::Applied), feedback.map(|f| f.outcome));
        let locked = vec![
            (w(1), rect(0, 0, 60, 120)),
            (w(2), rect(60, 0, 30, 120)),
            (w(3), rect(90, 0, 30, 120)),
        ];
        assert_eq!(locked, mgr.layout_sorted(space, screen));
        assert_eq!(locked, switched_frames(&mut mgr, space, screen, d, &all));
        assert_eq!(
            locked,
            switched_frames(&mut mgr, space, screen, ContextKey::Everything, &all)
        );
    }

    /// R6, L2. After the active context is deleted, its Space uses
    /// Everything's layout without logging an error until Unsorted shows.
    /// Other Spaces and the windows' other layouts are untouched.
    #[test]
    fn deleting_the_active_context_falls_back_to_everything_until_unsorted_shows() {
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let on1 = [w(1), w(2), w(3)];
        let on2 = [w(4), w(5)];
        let mut contexts = Contexts::new();
        let c_id = contexts.create("C").unwrap();
        let d_id = contexts.create("D").unwrap();
        let (c, d) = (ContextKey::Named(c_id), ContextKey::Named(d_id));
        let everything = ContextKey::Everything;
        let side_by_side = vec![
            (w(1), rect(0, 0, 40, 120)),
            (w(2), rect(40, 0, 40, 120)),
            (w(3), rect(80, 0, 40, 120)),
        ];
        let d1_frames = vec![(w(1), rect(0, 0, 120, 60)), (w(3), rect(0, 60, 120, 60))];
        let d2_frames = vec![(w(4), rect(0, 60, 120, 60)), (w(5), rect(0, 0, 120, 60))];

        switch(&mut mgr, space1, screen.size, everything, &on1);
        switch(&mut mgr, space2, screen.size, everything, &on2);
        switch(&mut mgr, space1, screen.size, c, &on1);
        move_window(&mut mgr, space1, w(1), Direction::Up);
        assert_eq!(
            d1_frames,
            switched_frames(&mut mgr, space1, screen, d, &[w(1), w(3)])
        );
        switch(&mut mgr, space2, screen.size, d, &on2);
        move_window(&mut mgr, space2, w(5), Direction::Up);
        assert_eq!(d2_frames, mgr.layout_sorted(space2, screen));
        switch(&mut mgr, space1, screen.size, c, &on1);
        contexts.switch_to(c).unwrap();
        assert_eq!(5, mgr.tree.layouts().len());

        contexts.delete(c_id).unwrap();
        mgr.remove_context_layouts(c_id);
        assert_eq!(ContextKey::Unsorted, contexts.active());
        assert_eq!(4, mgr.tree.layouts().len());
        assert_eq!(
            [(space1, d), (space2, d)].into_iter().collect::<HashSet<_>>(),
            context_keys(&mgr),
        );

        let mut frames = vec![];
        assert_eq!(0, count_errors(|| frames = mgr.layout_sorted(space1, screen)));
        assert_eq!(side_by_side, frames);
        assert_eq!(0, count_errors(|| frames = mgr.layout_sorted(space2, screen)));
        assert_eq!(d2_frames, frames);

        // The reactor shows Unsorted, whose layout starts from the one the
        // Space shows.
        let unsorted = ActiveContext {
            key: ContextKey::Unsorted,
            members: [w(2)].into_iter().collect(),
        };
        assert_eq!(
            0,
            count_errors(|| {
                _ = mgr.handle_event(LayoutEvent::SpaceExposed(space1, screen.size, unsorted));
            })
        );
        assert_eq!(0, count_errors(|| frames = mgr.layout_sorted(space1, screen)));
        assert_eq!(vec![(w(2), rect(0, 0, 120, 120))], frames);

        assert_eq!(d1_frames, shown_frames(&mut mgr, space1, screen, d));
        assert_eq!(side_by_side, shown_frames(&mut mgr, space1, screen, everything));
    }

    /// L2. Deleting a context removes every layout its mapping holds,
    /// including the one that a layout kind change just replaced.
    #[test]
    fn deleting_a_context_after_a_layout_kind_change_leaves_no_layout() {
        use LayoutCommand::*;
        let mut mgr = LayoutManager::new_for_test();
        mgr.set_config(&config_with_scroll(true, LayoutKind::Tree));
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c] = named_contexts(["C"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen.size, c, &all);
        _ = mgr.handle_command(Some(space), &[space], ChangeLayoutKind);
        assert_eq!(LayoutKind::Scroll, mgr.active_layout_kind(space));
        let scroll_layout = mgr.context_layouts[&(space, c)].active_layout();
        _ = mgr.viewport_mut(scroll_layout, screen.size.width);
        assert_eq!(2, mgr.context_layouts[&(space, c)].layouts().len());
        assert_eq!(3, mgr.tree.layouts().len());

        mgr.remove_context_layouts(context_id(c));
        assert!(mgr.context_layouts.is_empty());
        assert_eq!(1, mgr.tree.layouts().len());
        assert!(mgr.viewport(scroll_layout).is_none());
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(40, 0, 40, 120)),
                (w(3), rect(80, 0, 40, 120)),
            ],
            shown_frames(&mut mgr, space, screen, ContextKey::Everything),
        );
    }

    /// R6. Deleting a context ends a mouse move, resize, or drag on one of its
    /// layouts, so the next mouse event doesn't use a removed layout. One on
    /// another context's layout goes on.
    #[test]
    fn deleting_a_context_cancels_interactive_state_on_its_layouts() {
        let mut mgr = LayoutManager::new_for_test();
        let config = config_with_scroll(true, LayoutKind::Scroll);
        mgr.set_config(&config);
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let screen = rect(0, 0, 900, 600);
        let w = |idx| WindowId::new(1, idx);
        let on1 = [w(1), w(2), w(3)];
        let on2 = [w(4), w(5)];
        let [c, d] = named_contexts(["C", "D"]);
        let start = CGPoint::new(10.0, 10.0);
        let dragged = CGPoint::new(500.0, 300.0);

        switch(&mut mgr, space1, screen.size, ContextKey::Everything, &on1);
        switch(&mut mgr, space2, screen.size, ContextKey::Everything, &on2);
        switch(&mut mgr, space1, screen.size, c, &on1);
        switch(&mut mgr, space2, screen.size, d, &on2);

        // A move and a drag on C's layout, and a resize on D's.
        let c_layout = mgr.layout(space1);
        let c_node = mgr.tree.window_node(c_layout, w(1)).unwrap();
        assert!(mgr.begin_interactive_move(space1, w(1), c_node, start));
        mgr.interactive_drag = Some(InteractiveDrag {
            layout_id: c_layout,
            source_wid: w(1),
            source_node: c_node,
            start_mouse: start,
            drag_active: false,
            hover_target: None,
            current_action: None,
            preview: DragPreviewState {
                original_frames: HashMap::default(),
                last_action: None,
            },
        });
        let d_layout = mgr.layout(space2);
        let d_window = mgr.tree.window_node(d_layout, w(4)).unwrap();
        let d_column = mgr.tree.column_of(d_layout, d_window).unwrap();
        let right = ResizeEdge(ResizeEdge::RIGHT);
        assert!(mgr.begin_interactive_resize(d_column, d_window, right, start));

        mgr.remove_context_layouts(context_id(c));
        assert!(mgr.interactive_move.is_none());
        assert!(mgr.interactive_drag.is_none());
        assert!(mgr.interactive_resize.is_some());
        assert!(!mgr.update_interactive_move(dragged, screen, &config));
        assert!(matches!(
            mgr.update_interactive_drag(dragged, screen, &config),
            DragUpdate::NoChange
        ));
        _ = mgr.update_interactive_resize(dragged, screen);

        mgr.remove_context_layouts(context_id(d));
        assert!(!mgr.has_interactive_state());
        assert!(!mgr.update_interactive_resize(start, screen));
    }

    /// L2. A `layout.ron` saved with context layouts restores them. Dropping
    /// the ids that `contexts.json` no longer has removes exactly their
    /// layouts, on every Space and size.
    #[test]
    fn a_restored_layout_drops_the_layouts_of_deleted_contexts() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let small = rect(0, 0, 120, 120);
        let wide = rect(0, 0, 240, 120);
        let w = |idx| WindowId::new(1, idx);
        let on1 = [w(1), w(2), w(3)];
        let on2 = [w(4), w(5)];
        let mut contexts = Contexts::new();
        let c_id = contexts.create("C").unwrap();
        let d_id = contexts.create("D").unwrap();
        let (c, d) = (ContextKey::Named(c_id), ContextKey::Named(d_id));
        let everything = ContextKey::Everything;
        let unsorted = ContextKey::Unsorted;

        switch(&mut mgr, space1, small.size, everything, &on1);
        switch(&mut mgr, space2, small.size, everything, &on2);
        switch(&mut mgr, space1, small.size, c, &on1);
        move_window(&mut mgr, space1, w(1), Direction::Up);
        switch(&mut mgr, space1, wide.size, c, &on1);
        move_window(&mut mgr, space1, w(3), Direction::Up);
        switch(&mut mgr, space1, small.size, c, &on1);
        switch(&mut mgr, space1, small.size, d, &[w(2), w(3)]);
        switch(&mut mgr, space1, small.size, unsorted, &[w(3)]);
        switch(&mut mgr, space2, small.size, d, &on2);
        move_window(&mut mgr, space2, w(5), Direction::Up);
        // w6 is only in D's layout.
        _ = mgr.handle_event(WindowAdded(space2, w(6), win_info()));
        assert_eq!(
            vec![
                (w(4), rect(0, 80, 120, 40)),
                (w(5), rect(0, 0, 120, 40)),
                (w(6), rect(0, 40, 120, 40)),
            ],
            mgr.layout_sorted(space2, small),
        );
        switch(&mut mgr, space2, small.size, c, &on2);
        assert_eq!(8, mgr.tree.layouts().len());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.ron");
        mgr.save(path.clone()).unwrap();
        contexts.delete(d_id).unwrap();
        let json = serde_json::to_string(&contexts).unwrap();
        let mut contexts: Contexts = serde_json::from_str(&json).unwrap();

        let mut restored = LayoutManager::load(path, default_config()).unwrap();
        assert_eq!(8, restored.tree.layouts().len());
        restored.retain_context_layouts(|id| contexts.get(id).is_some());
        assert_eq!(
            [(space1, unsorted), (space1, c), (space2, c)]
                .into_iter()
                .collect::<HashSet<_>>(),
            context_keys(&restored),
        );
        assert_eq!(6, restored.tree.layouts().len());
        assert!(!restored.tree.has_window(w(6)));
        assert_eq!(
            [w(1), w(2), w(3), w(4), w(5)].into_iter().collect::<BTreeSet<_>>(),
            restored.all_windows(),
        );
        _ = restored.handle_event(WindowRemoved(w(6)));

        let errors = count_errors(|| {
            assert_eq!(
                vec![
                    (w(1), rect(0, 0, 120, 60)),
                    (w(2), rect(0, 60, 60, 60)),
                    (w(3), rect(60, 60, 60, 60)),
                ],
                switched_frames(&mut restored, space1, small, c, &on1),
            );
            assert_eq!(
                vec![
                    (w(1), rect(0, 0, 240, 40)),
                    (w(2), rect(0, 80, 240, 40)),
                    (w(3), rect(0, 40, 240, 40)),
                ],
                switched_frames(&mut restored, space1, wide, c, &on1),
            );
            assert_eq!(
                vec![(w(3), rect(0, 0, 120, 120))],
                switched_frames(&mut restored, space1, small, unsorted, &[w(3)]),
            );
            assert_eq!(
                vec![
                    (w(1), rect(0, 0, 40, 120)),
                    (w(2), rect(40, 0, 40, 120)),
                    (w(3), rect(80, 0, 40, 120)),
                ],
                switched_frames(&mut restored, space1, small, everything, &on1),
            );
            assert_eq!(
                vec![(w(4), rect(0, 60, 120, 60)), (w(5), rect(0, 0, 120, 60))],
                switched_frames(&mut restored, space2, small, c, &on2),
            );
            assert_eq!(
                vec![(w(4), rect(0, 0, 60, 120)), (w(5), rect(60, 0, 60, 120))],
                switched_frames(&mut restored, space2, small, everything, &on2),
            );
        });
        assert_eq!(0, errors);

        // A new context named like the deleted one gets a new id and starts
        // from the layout the Space shows, not from D's old layout.
        let new_d = ContextKey::Named(contexts.create("d").unwrap());
        assert_ne!(d, new_d);
        assert_eq!(
            vec![(w(4), rect(0, 0, 60, 120)), (w(5), rect(60, 0, 60, 120))],
            switched_frames(&mut restored, space2, small, new_d, &on2),
        );
    }

    /// L1, L9. Turning off scroll layouts converts every context's layout on
    /// every Space, and each keeps exactly its own windows.
    #[test]
    fn turning_off_scroll_layouts_keeps_each_contexts_windows() {
        let mut mgr = LayoutManager::new_for_test();
        mgr.set_config(&config_with_scroll(true, LayoutKind::Scroll));
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let on1 = [w(1), w(2), w(3)];
        let on2 = [w(4), w(5)];
        let [c, d] = named_contexts(["C", "D"]);
        let everything = ContextKey::Everything;

        switch(&mut mgr, space1, screen.size, everything, &on1);
        switch(&mut mgr, space1, screen.size, c, &[w(1), w(2)]);
        switch(&mut mgr, space1, screen.size, d, &[w(2), w(3)]);
        switch(&mut mgr, space2, screen.size, everything, &on2);
        switch(&mut mgr, space2, screen.size, c, &on2);
        let keys = [
            (space1, everything),
            (space1, c),
            (space1, d),
            (space2, everything),
            (space2, c),
        ];
        let kinds = |mgr: &LayoutManager| {
            keys.map(|(space, key)| {
                mgr.tree.layout_kind(mgr.mapping(space, key).unwrap().active_layout())
            })
        };
        assert_eq!([LayoutKind::Scroll; 5], kinds(&mgr));

        mgr.set_config(&config_with_scroll(false, LayoutKind::Tree));
        assert_eq!([LayoutKind::Tree; 5], kinds(&mgr));
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(40, 0, 40, 120)),
                (w(3), rect(80, 0, 40, 120)),
            ],
            shown_frames(&mut mgr, space1, screen, everything),
        );
        assert_eq!(
            vec![(w(1), rect(0, 0, 60, 120)), (w(2), rect(60, 0, 60, 120))],
            shown_frames(&mut mgr, space1, screen, c),
        );
        assert_eq!(
            vec![(w(2), rect(0, 0, 60, 120)), (w(3), rect(60, 0, 60, 120))],
            shown_frames(&mut mgr, space1, screen, d),
        );
        let halves = vec![(w(4), rect(0, 0, 60, 120)), (w(5), rect(60, 0, 60, 120))];
        assert_eq!(halves, shown_frames(&mut mgr, space2, screen, everything));
        assert_eq!(halves, shown_frames(&mut mgr, space2, screen, c));
    }

    /// L1. A context's scroll layout for another screen size becomes a tree
    /// when that size is shown with scroll layouts off.
    #[test]
    fn a_contexts_remembered_scroll_layout_becomes_a_tree_when_shown() {
        use LayoutCommand::*;
        let mut mgr = LayoutManager::new_for_test();
        mgr.set_config(&config_with_scroll(true, LayoutKind::Tree));
        let space = SpaceId::new(1);
        let small = rect(0, 0, 120, 120);
        let wide = rect(0, 0, 240, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c] = named_contexts(["C"]);

        switch(&mut mgr, space, small.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, small.size, c, &all);
        move_window(&mut mgr, space, w(1), Direction::Up);
        switch(&mut mgr, space, wide.size, c, &all);
        move_window(&mut mgr, space, w(3), Direction::Up);
        _ = mgr.handle_command(Some(space), &[space], ChangeLayoutKind);
        assert_eq!(LayoutKind::Scroll, mgr.active_layout_kind(space));
        let c_small = vec![
            (w(1), rect(0, 0, 120, 60)),
            (w(2), rect(0, 60, 60, 60)),
            (w(3), rect(60, 60, 60, 60)),
        ];
        assert_eq!(c_small, switched_frames(&mut mgr, space, small, c, &all));

        mgr.set_config(&config_with_scroll(false, LayoutKind::Tree));
        show(&mut mgr, space, wide.size, c);
        assert_eq!(LayoutKind::Tree, mgr.active_layout_kind(space));
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 80, 120)),
                (w(2), rect(160, 0, 80, 120)),
                (w(3), rect(80, 0, 80, 120)),
            ],
            mgr.layout_sorted(space, wide),
        );
        assert_eq!(c_small, shown_frames(&mut mgr, space, small, c));
    }

    /// L1. A restored manager whose config turns scroll layouts off converts
    /// its context layouts too.
    #[test]
    fn restoring_with_scroll_layouts_off_converts_context_layouts() {
        let mut mgr = LayoutManager::new_for_test();
        mgr.set_config(&config_with_scroll(true, LayoutKind::Scroll));
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c] = named_contexts(["C"]);
        let everything = ContextKey::Everything;
        let unsorted = ContextKey::Unsorted;

        switch(&mut mgr, space, screen.size, everything, &all);
        switch(&mut mgr, space, screen.size, c, &[w(1), w(2)]);
        switch(&mut mgr, space, screen.size, unsorted, &[w(3)]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.ron");
        mgr.save(path.clone()).unwrap();

        let config_off = config_with_scroll(false, LayoutKind::Tree);
        let mut restored = LayoutManager::load(path, config_off.clone()).unwrap();
        let kinds = |mgr: &LayoutManager| {
            [everything, c, unsorted]
                .map(|key| mgr.tree.layout_kind(mgr.mapping(space, key).unwrap().active_layout()))
        };
        assert_eq!([LayoutKind::Scroll; 3], kinds(&restored));
        restored.set_config(&config_off);
        assert_eq!([LayoutKind::Tree; 3], kinds(&restored));

        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(40, 0, 40, 120)),
                (w(3), rect(80, 0, 40, 120)),
            ],
            shown_frames(&mut restored, space, screen, everything),
        );
        assert_eq!(
            vec![(w(1), rect(0, 0, 60, 120)), (w(2), rect(60, 0, 60, 120))],
            shown_frames(&mut restored, space, screen, c),
        );
        assert_eq!(
            vec![(w(3), rect(0, 0, 120, 120))],
            shown_frames(&mut restored, space, screen, unsorted),
        );
    }

    /// R28. With no context active, every Space keeps only Everything's
    /// layouts, and nothing logs an error.
    #[test]
    fn without_contexts_only_everythings_layouts_exist() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        mgr.set_config(&config_with_scroll(true, LayoutKind::Tree));
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let small = rect(0, 0, 120, 120);
        let wide = rect(0, 0, 240, 120);
        let w = |idx| WindowId::new(1, idx);
        let on1 = [w(1), w(2), w(3)];
        let on2 = [w(4), w(5)];
        let everything = ContextKey::Everything;

        let errors = count_errors(|| {
            switch(&mut mgr, space1, small.size, everything, &on1);
            switch(&mut mgr, space2, small.size, everything, &on2);
            move_window(&mut mgr, space1, w(1), Direction::Up);
            switch(&mut mgr, space1, wide.size, everything, &on1);
            move_window(&mut mgr, space1, w(3), Direction::Up);
            _ = mgr.handle_command(Some(space1), &[space1], NextLayout);
            _ = mgr.handle_command(Some(space1), &[space1], PrevLayout);
            _ = mgr.handle_event(WindowFocused(vec![space1], w(2)));
            _ = mgr.handle_command(Some(space1), &[space1], ToggleWindowFloating);
            _ = mgr.handle_command(Some(space1), &[space1], ToggleWindowFloating);
            _ = mgr.handle_event(WindowAdded(space2, w(6), win_info()));
            _ = mgr.handle_event(WindowRemoved(w(4)));
            _ = mgr.handle_command(Some(space2), &[space2], ChangeLayoutKind);
            // Everything ignores the members it's given.
            let context = ActiveContext {
                key: everything,
                members: on1.into_iter().collect(),
            };
            _ = mgr.handle_event(SpaceExposed(space1, small.size, context));
            mgr.set_config(&config_with_scroll(false, LayoutKind::Tree));
        });
        assert_eq!(0, errors);
        assert!(mgr.context_layouts.is_empty());
        assert!(mgr.active_contexts.values().all(|&key| key == everything));
        assert!(mgr.serialize_to_string().contains(",context_layouts:{},"));

        // Floating took w2 out of every size's layout, so it comes back last
        // where it wasn't unfloated.
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 120, 40)),
                (w(2), rect(0, 80, 120, 40)),
                (w(3), rect(0, 40, 120, 40)),
            ],
            switched_frames(&mut mgr, space1, small, everything, &on1),
        );
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 240, 40)),
                (w(2), rect(0, 80, 240, 40)),
                (w(3), rect(0, 40, 240, 40)),
            ],
            switched_frames(&mut mgr, space1, wide, everything, &on1),
        );
        assert_eq!(LayoutKind::Tree, mgr.active_layout_kind(space2));
        assert_eq!(
            vec![(w(5), rect(0, 0, 60, 120)), (w(6), rect(60, 0, 60, 120))],
            shown_frames(&mut mgr, space2, small, everything),
        );
    }

    /// L2. `NextLayout` and `PrevLayout` work under Everything, do nothing
    /// under Unsorted, and work again when Everything shows again.
    #[test]
    fn next_layout_works_under_everything_and_not_under_unsorted() {
        use LayoutCommand::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let small = rect(0, 0, 120, 120);
        let wide = rect(0, 0, 240, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let everything = ContextKey::Everything;
        let unsorted = ContextKey::Unsorted;

        switch(&mut mgr, space, small.size, everything, &all);
        move_window(&mut mgr, space, w(1), Direction::Up);
        switch(&mut mgr, space, wide.size, everything, &all);
        move_window(&mut mgr, space, w(3), Direction::Up);
        let l2_wide = vec![
            (w(1), rect(0, 0, 240, 40)),
            (w(2), rect(0, 80, 240, 40)),
            (w(3), rect(0, 40, 240, 40)),
        ];
        assert_eq!(l2_wide, mgr.layout_sorted(space, wide));
        _ = mgr.handle_command(Some(space), &[space], NextLayout);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 240, 60)),
                (w(2), rect(0, 60, 120, 60)),
                (w(3), rect(120, 60, 120, 60)),
            ],
            mgr.layout_sorted(space, wide),
        );
        _ = mgr.handle_command(Some(space), &[space], PrevLayout);
        assert_eq!(l2_wide, mgr.layout_sorted(space, wide));

        // Give Unsorted two layouts.
        switch(&mut mgr, space, wide.size, unsorted, &all);
        move_window(&mut mgr, space, w(1), Direction::Down);
        assert_eq!(
            vec![
                (w(1), rect(0, 40, 240, 40)),
                (w(2), rect(0, 80, 240, 40)),
                (w(3), rect(0, 0, 240, 40)),
            ],
            mgr.layout_sorted(space, wide),
        );
        switch(&mut mgr, space, small.size, unsorted, &all);
        move_window(&mut mgr, space, w(1), Direction::Down);
        assert_eq!(2, mgr.context_layouts[&(space, unsorted)].layouts().len());
        let unsorted_small = vec![
            (w(1), rect(0, 80, 120, 40)),
            (w(2), rect(0, 40, 120, 40)),
            (w(3), rect(0, 0, 120, 40)),
        ];
        assert_eq!(unsorted_small, mgr.layout_sorted(space, small));
        _ = mgr.handle_command(Some(space), &[space], NextLayout);
        assert_eq!(unsorted_small, mgr.layout_sorted(space, small));
        _ = mgr.handle_command(Some(space), &[space], PrevLayout);
        assert_eq!(unsorted_small, mgr.layout_sorted(space, small));

        assert_eq!(
            vec![
                (w(1), rect(0, 0, 120, 60)),
                (w(2), rect(0, 60, 60, 60)),
                (w(3), rect(60, 60, 60, 60)),
            ],
            switched_frames(&mut mgr, space, small, everything, &all),
        );
        _ = mgr.handle_command(Some(space), &[space], NextLayout);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 120, 40)),
                (w(2), rect(0, 80, 120, 40)),
                (w(3), rect(0, 40, 120, 40)),
            ],
            mgr.layout_sorted(space, small),
        );
    }

    /// L9. With no context active, changing the layout kind at one screen size
    /// leaves the windows in the layout of another size.
    #[test]
    fn changing_the_layout_kind_under_everything_keeps_another_sizes_layout() {
        use LayoutCommand::*;
        let mut mgr = LayoutManager::new_for_test();
        mgr.set_config(&config_with_scroll(true, LayoutKind::Tree));
        let space = SpaceId::new(1);
        let small = rect(0, 0, 120, 120);
        let wide = rect(0, 0, 240, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let everything = ContextKey::Everything;

        switch(&mut mgr, space, small.size, everything, &all);
        move_window(&mut mgr, space, w(1), Direction::Up);
        switch(&mut mgr, space, wide.size, everything, &all);
        move_window(&mut mgr, space, w(3), Direction::Up);
        switch(&mut mgr, space, small.size, everything, &all);
        _ = mgr.handle_command(Some(space), &[space], ChangeLayoutKind);
        assert_eq!(LayoutKind::Scroll, mgr.active_layout_kind(space));

        assert_eq!(
            vec![
                (w(1), rect(0, 0, 240, 40)),
                (w(2), rect(0, 80, 240, 40)),
                (w(3), rect(0, 40, 240, 40)),
            ],
            shown_frames(&mut mgr, space, wide, everything),
        );
    }

    /// Whether some layout holds two nodes for one window.
    fn has_duplicate_nodes(mgr: &LayoutManager) -> bool {
        mgr.tree.layouts().any(|layout| {
            let windows: Vec<WindowId> = mgr
                .tree
                .root(layout)
                .traverse_postorder(mgr.tree.map())
                .filter_map(|node| mgr.tree.window_at(node))
                .collect();
            windows.len() != windows.iter().collect::<HashSet<_>>().len()
        })
    }

    /// L9. Floating a window and changing the layout kind under Everything
    /// leave the window's nodes in C's and D's layouts, before the windows
    /// are sent again. No layout gets two nodes for one window.
    #[test]
    fn floating_and_changing_the_layout_kind_under_everything_keep_context_nodes() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        mgr.set_config(&config_with_scroll(true, LayoutKind::Tree));
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);
        let everything = ContextKey::Everything;
        let c_frames = vec![
            (w(1), rect(0, 0, 120, 60)),
            (w(2), rect(0, 60, 60, 60)),
            (w(3), rect(60, 60, 60, 60)),
        ];
        let d_frames = vec![
            (w(1), rect(0, 60, 60, 60)),
            (w(2), rect(0, 0, 120, 60)),
            (w(3), rect(60, 60, 60, 60)),
        ];

        switch(&mut mgr, space, screen.size, everything, &all);
        switch(&mut mgr, space, screen.size, d, &all);
        move_window(&mut mgr, space, w(2), Direction::Up);
        switch(&mut mgr, space, screen.size, everything, &all);
        switch(&mut mgr, space, screen.size, c, &all);
        move_window(&mut mgr, space, w(1), Direction::Up);
        switch(&mut mgr, space, screen.size, everything, &all);

        _ = mgr.handle_event(WindowFocused(vec![space], w(2)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(
            vec![(w(1), rect(0, 0, 60, 120)), (w(3), rect(60, 0, 60, 120))],
            mgr.layout_sorted(space, screen),
        );
        assert_eq!(c_frames, shown_frames(&mut mgr, space, screen, c));
        assert_eq!(d_frames, shown_frames(&mut mgr, space, screen, d));

        show(&mut mgr, space, screen.size, everything);
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        _ = mgr.handle_command(Some(space), &[space], ChangeLayoutKind);
        assert_eq!(LayoutKind::Scroll, mgr.active_layout_kind(space));
        let everything_layout = mgr.layout_mapping[&space].active_layout();
        for wid in all {
            assert!(mgr.tree.window_node(everything_layout, wid).is_some());
        }
        assert!(!has_duplicate_nodes(&mgr));
        assert_eq!(c_frames, shown_frames(&mut mgr, space, screen, c));
        assert_eq!(d_frames, shown_frames(&mut mgr, space, screen, d));
        assert!(!has_duplicate_nodes(&mgr));
    }

    /// L3. A context whose first layout is made with no members starts
    /// empty.
    #[test]
    fn a_context_with_no_members_starts_with_an_empty_layout() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c] = named_contexts(["C"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        assert_eq!(NO_FRAMES, shown_frames(&mut mgr, space, screen, c));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, 1, vec![(w(2), win_info())]));
        assert_eq!(
            vec![(w(2), rect(0, 0, 120, 120))],
            mgr.layout_sorted(space, screen)
        );
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(40, 0, 40, 120)),
                (w(3), rect(80, 0, 40, 120)),
            ],
            shown_frames(&mut mgr, space, screen, ContextKey::Everything),
        );
    }

    /// L1, L3. A context can be the first thing a Space shows. Its layout and
    /// Everything's start empty and fill separately.
    #[test]
    fn a_context_can_be_shown_first_on_a_new_space() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let [c] = named_contexts(["C"]);

        let context = ActiveContext {
            key: c,
            members: [w(1), w(2)].into_iter().collect(),
        };
        let errors = count_errors(|| {
            _ = mgr.handle_event(SpaceExposed(space, screen.size, context));
        });
        assert_eq!(0, errors);
        assert!(mgr.layout_mapping.contains_key(&space));
        assert_eq!(
            [(space, c)].into_iter().collect::<HashSet<_>>(),
            context_keys(&mgr)
        );
        assert_eq!(NO_FRAMES, mgr.layout_sorted(space, screen));

        let halves = vec![(w(1), rect(0, 0, 60, 120)), (w(2), rect(60, 0, 60, 120))];
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, 1, make_windows(1, 2)));
        assert_eq!(halves, mgr.layout_sorted(space, screen));
        assert_eq!(
            NO_FRAMES,
            shown_frames(&mut mgr, space, screen, ContextKey::Everything)
        );
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, 1, make_windows(1, 3)));
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(40, 0, 40, 120)),
                (w(3), rect(80, 0, 40, 120)),
            ],
            mgr.layout_sorted(space, screen),
        );
        assert_eq!(halves, shown_frames(&mut mgr, space, screen, c));
    }

    /// L3. The members passed with `SpaceExposed` shape only a context's first
    /// layout on the Space.
    #[test]
    fn members_shape_only_a_contexts_first_layout() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c] = named_contexts(["C"]);
        let thirds = vec![
            (w(1), rect(0, 0, 40, 120)),
            (w(2), rect(40, 0, 40, 120)),
            (w(3), rect(80, 0, 40, 120)),
        ];

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        assert_eq!(thirds, switched_frames(&mut mgr, space, screen, c, &all));
        show(&mut mgr, space, screen.size, ContextKey::Everything);
        let context = ActiveContext {
            key: c,
            members: [w(1)].into_iter().collect(),
        };
        _ = mgr.handle_event(SpaceExposed(space, screen.size, context));
        assert_eq!(thirds, mgr.layout_sorted(space, screen));
    }

    /// R29, L2. Dropping named contexts never removes Unsorted's layouts,
    /// and dropping a context that has no layouts changes nothing.
    #[test]
    fn dropping_context_layouts_keeps_unsorted() {
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, never_shown] = named_contexts(["C", "Never shown"]);
        let unsorted = ContextKey::Unsorted;

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen.size, unsorted, &[w(3)]);
        switch(&mut mgr, space, screen.size, c, &all);
        let keys = [(space, unsorted), (space, c)].into_iter().collect::<HashSet<_>>();
        assert_eq!(3, mgr.tree.layouts().len());

        mgr.remove_context_layouts(context_id(never_shown));
        assert_eq!(keys, context_keys(&mgr));
        assert_eq!(3, mgr.tree.layouts().len());
        mgr.retain_context_layouts(|_| true);
        assert_eq!(keys, context_keys(&mgr));
        assert_eq!(3, mgr.tree.layouts().len());
        // C was cloned from Unsorted's layout, so w3 comes first.
        assert_eq!(
            vec![
                (w(1), rect(40, 0, 40, 120)),
                (w(2), rect(80, 0, 40, 120)),
                (w(3), rect(0, 0, 40, 120)),
            ],
            mgr.layout_sorted(space, screen),
        );

        mgr.retain_context_layouts(|_| false);
        assert_eq!(
            [(space, unsorted)].into_iter().collect::<HashSet<_>>(),
            context_keys(&mgr),
        );
        assert_eq!(2, mgr.tree.layouts().len());
        show(&mut mgr, space, screen.size, unsorted);
        let mut frames = vec![];
        assert_eq!(0, count_errors(|| frames = mgr.layout_sorted(space, screen)));
        assert_eq!(vec![(w(3), rect(0, 0, 120, 120))], frames);
    }

    /// A window that closes leaves every layout it has a node in, whatever the
    /// context.
    #[test]
    fn a_closed_window_leaves_every_contexts_layout() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let [c, d] = named_contexts(["C", "D"]);

        switch(&mut mgr, space, screen.size, ContextKey::Everything, &all);
        switch(&mut mgr, space, screen.size, c, &all);
        move_window(&mut mgr, space, w(1), Direction::Up);
        switch(&mut mgr, space, screen.size, d, &[w(2), w(3)]);

        _ = mgr.handle_event(WindowRemoved(w(2)));
        assert!(!mgr.tree.has_window(w(2)));
        assert_eq!(
            vec![(w(3), rect(0, 0, 120, 120))],
            mgr.layout_sorted(space, screen)
        );
        assert_eq!(
            vec![(w(1), rect(0, 0, 120, 60)), (w(3), rect(0, 60, 120, 60))],
            shown_frames(&mut mgr, space, screen, c),
        );
        assert_eq!(
            vec![(w(1), rect(0, 0, 60, 120)), (w(3), rect(60, 0, 60, 120))],
            shown_frames(&mut mgr, space, screen, ContextKey::Everything),
        );
    }

    /// The windows of an app that quits leave every context's layout.
    #[test]
    fn a_quit_apps_windows_leave_every_contexts_layout() {
        use LayoutEvent::*;
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let a = WindowId::new(1, 1);
        let b1 = WindowId::new(2, 1);
        let b2 = WindowId::new(2, 2);
        let [c, d] = named_contexts(["C", "D"]);
        let everything = ContextKey::Everything;
        let show_windows = |mgr: &mut LayoutManager, key: ContextKey, windows: &[WindowId]| {
            let context = ActiveContext {
                key,
                members: windows.iter().copied().collect(),
            };
            _ = mgr.handle_event(SpaceExposed(space, screen.size, context));
            for pid in [1, 2] {
                let windows = windows
                    .iter()
                    .filter(|wid| wid.pid == pid)
                    .map(|&wid| (wid, win_info()))
                    .collect();
                _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows));
            }
        };

        for quit in [AppClosed(2), AppsRunningUpdated([1].into_iter().collect())] {
            let mut mgr = LayoutManager::new_for_test();
            show_windows(&mut mgr, everything, &[a, b1, b2]);
            show_windows(&mut mgr, c, &[a, b1]);
            show_windows(&mut mgr, d, &[b1, b2]);
            assert_eq!(
                vec![(b1, rect(0, 0, 60, 120)), (b2, rect(60, 0, 60, 120))],
                mgr.layout_sorted(space, screen),
            );

            _ = mgr.handle_event(quit);
            assert_eq!(NO_FRAMES, mgr.layout_sorted(space, screen));
            assert_eq!(
                vec![(a, rect(0, 0, 120, 120))],
                shown_frames(&mut mgr, space, screen, c)
            );
            assert_eq!(
                vec![(a, rect(0, 0, 120, 120))],
                shown_frames(&mut mgr, space, screen, everything)
            );
        }
    }

    /// R4, R6, L3. Layouts belong to a context's id, not its name. Renaming
    /// keeps them, and a new context with a deleted one's name, in any case,
    /// starts from the layout the Space shows.
    #[test]
    fn a_context_recreated_under_a_case_variant_name_starts_fresh() {
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let screen = rect(0, 0, 120, 120);
        let w = |idx| WindowId::new(1, idx);
        let all = [w(1), w(2), w(3)];
        let everything = ContextKey::Everything;
        let mut contexts = Contexts::new();
        let work_id = contexts.create("Work").unwrap();
        let work = ContextKey::Named(work_id);
        let work_frames = vec![
            (w(1), rect(0, 0, 120, 60)),
            (w(2), rect(0, 60, 60, 60)),
            (w(3), rect(60, 60, 60, 60)),
        ];

        switch(&mut mgr, space, screen.size, everything, &all);
        switch(&mut mgr, space, screen.size, work, &all);
        move_window(&mut mgr, space, w(1), Direction::Up);
        assert_eq!(work_frames, mgr.layout_sorted(space, screen));

        contexts.rename(work_id, "WORK").unwrap();
        assert!(contexts.create("work").is_err());
        assert!(contexts.create("everything").is_err());
        assert!(contexts.create("UNSORTED").is_err());
        switch(&mut mgr, space, screen.size, everything, &all);
        assert_eq!(work_frames, switched_frames(&mut mgr, space, screen, work, &all));

        switch(&mut mgr, space, screen.size, everything, &all);
        contexts.delete(work_id).unwrap();
        mgr.remove_context_layouts(work_id);
        let again = ContextKey::Named(contexts.create("work").unwrap());
        assert_ne!(work, again);
        assert_eq!(
            vec![
                (w(1), rect(0, 0, 40, 120)),
                (w(2), rect(40, 0, 40, 120)),
                (w(3), rect(80, 0, 40, 120)),
            ],
            switched_frames(&mut mgr, space, screen, again, &all),
        );
    }

    /// R28. Layouts saved before contexts existed restore with no context
    /// layouts, and each Space shows Everything's layout without an error.
    #[test]
    fn layouts_saved_before_contexts_restore_to_everything() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
        let mut restored = 0;
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("ron") {
                continue;
            }
            let mgr: LayoutManager = ron::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert!(mgr.context_layouts.is_empty(), "{path:?}");
            assert!(mgr.active_contexts.is_empty(), "{path:?}");
            assert!(!mgr.layout_mapping.is_empty(), "{path:?}");
            for (&space, mapping) in &mgr.layout_mapping {
                let mut layout = None;
                assert_eq!(0, count_errors(|| layout = mgr.try_layout(space)), "{path:?}");
                assert_eq!(Some(mapping.active_layout()), layout, "{path:?}");
            }
            assert!(
                mgr.serialize_to_string().contains(",context_layouts:{},"),
                "{path:?}"
            );
            restored += 1;
        }
        assert!(restored >= 4, "only {restored} snapshots in {dir:?}");
    }
}

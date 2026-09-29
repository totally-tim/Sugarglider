// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{HashMap, HashSet};
use std::{iter, mem};

use objc2_core_foundation::{CGRect, CGSize};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::selection::Selection;
use super::size::{ContainerKind, Direction, Masses, Size};
use super::tree::{self, StashPosition, Tree};
use super::window::Window;
use crate::actor::app::{WindowId, pid_t};
use crate::config::Config;
use crate::model::tree::{NodeId, NodeMap, OwnedNode};

/// The layout tree.
///
/// All interactions with the data model happen through the public APIs on this
/// type.
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LayoutKind {
    #[default]
    Tree,
    Scroll,
}

#[derive(Serialize, Deserialize)]
pub struct LayoutTree {
    tree: Tree<Components>,
    layout_roots: slotmap::SlotMap<LayoutId, OwnedNode>,
    #[serde(default)]
    layout_kinds: slotmap::SecondaryMap<LayoutId, LayoutKind>,
    /// Nodes taken out of a layout so its gap closes, kept for the window's
    /// return. A window that left a layout for another screen comes back to
    /// the place it held there.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    stashed: HashMap<(LayoutId, WindowId), StashedNode>,
}

/// A node kept out of its layout, with the place it will return to.
#[derive(Serialize, Deserialize)]
struct StashedNode {
    node: OwnedNode,
    /// The weight the node had in its parent.
    weight: f32,
    /// The parent it was under, and its neighbors, which may be gone by the
    /// time the window returns.
    parent: NodeId,
    prev: Option<NodeId>,
    next: Option<NodeId>,
}

slotmap::new_key_type! {
    pub struct LayoutId;
}

impl LayoutTree {
    pub fn new() -> LayoutTree {
        LayoutTree {
            tree: Tree::with_observer(Components::default()),
            layout_roots: Default::default(),
            layout_kinds: Default::default(),
            stashed: Default::default(),
        }
    }

    pub fn create_layout(&mut self) -> LayoutId {
        self.create_layout_with_kind(LayoutKind::Tree)
    }

    pub fn create_scroll_layout(&mut self) -> LayoutId {
        self.create_layout_with_kind(LayoutKind::Scroll)
    }

    fn create_layout_with_kind(&mut self, kind: LayoutKind) -> LayoutId {
        let root = OwnedNode::new_root_in(&mut self.tree, "layout_root");
        let id = self.layout_roots.insert(root);
        self.layout_kinds.insert(id, kind);
        if kind == LayoutKind::Scroll {
            self.tree
                .data
                .size
                .set_kind(self.layout_roots[id].id(), ContainerKind::Horizontal);
        }
        id
    }

    pub fn remove_layout(&mut self, layout: LayoutId) {
        self.drop_stashes(|other, _| other == layout);
        self.layout_roots.remove(layout).unwrap().remove(&mut self.tree);
        self.layout_kinds.remove(layout);
    }

    pub fn layouts(&self) -> impl ExactSizeIterator<Item = LayoutId> {
        self.layout_roots.keys()
    }

    pub fn root(&self, layout: LayoutId) -> NodeId {
        self.layout_roots[layout].id()
    }

    pub fn layout_kind(&self, layout: LayoutId) -> LayoutKind {
        self.layout_kinds.get(layout).copied().unwrap_or_default()
    }

    pub fn is_scroll_layout(&self, layout: LayoutId) -> bool {
        self.layout_kind(layout) == LayoutKind::Scroll
    }

    /// Returns true if `node` is the root node of a scroll layout.
    fn is_scroll_root(&self, node: NodeId) -> bool {
        for (layout_id, &kind) in &self.layout_kinds {
            if kind == LayoutKind::Scroll
                && self.layout_roots.get(layout_id).map(|r| r.id()) == Some(node)
            {
                return true;
            }
        }
        false
    }

    pub fn add_window_to_scroll_column(
        &mut self,
        layout: LayoutId,
        wid: WindowId,
        new_column: bool,
    ) -> NodeId {
        self.add_window_to_scroll_column_with_visible(layout, wid, new_column, 2)
    }

    pub fn add_window_to_scroll_column_with_visible(
        &mut self,
        layout: LayoutId,
        wid: WindowId,
        new_column: bool,
        visible_columns: u32,
    ) -> NodeId {
        if let Some(node) = self.unstash_window(layout, wid) {
            return node;
        }
        let root = self.root(layout);
        let selection = self.selection(layout);
        let weight = 1.0 / visible_columns.max(1) as f32;
        if new_column {
            let column = if selection == root {
                self.tree.mk_node().push_back(root)
            } else {
                let parent = selection
                    .ancestors(&self.tree.map)
                    .find(|&n| n.parent(&self.tree.map) == Some(root))
                    .unwrap_or(selection);
                self.tree.mk_node().insert_after(parent)
            };
            self.tree.data.size.set_kind(column, ContainerKind::Vertical);
            self.tree.data.size.set_weight(column, weight, &self.tree.map);
            let node = self.tree.mk_node().push_back(column);
            self.tree.data.window.set_window(node, wid);
            node
        } else {
            let column = selection
                .ancestors(&self.tree.map)
                .find(|&n| n.parent(&self.tree.map) == Some(root))
                .unwrap_or(root);
            if column == root {
                let col = self.tree.mk_node().push_back(root);
                self.tree.data.size.set_kind(col, ContainerKind::Vertical);
                self.tree.data.size.set_weight(col, weight, &self.tree.map);
                let node = self.tree.mk_node().push_back(col);
                self.tree.data.window.set_window(node, wid);
                node
            } else {
                let node = self.tree.mk_node().insert_after(selection);
                self.tree.data.window.set_window(node, wid);
                node
            }
        }
    }

    pub fn column_of(&self, layout: LayoutId, node: NodeId) -> Option<NodeId> {
        let root = self.root(layout);
        node.ancestors(&self.tree.map).find(|&n| n.parent(&self.tree.map) == Some(root))
    }

    pub fn columns(&self, layout: LayoutId) -> Vec<NodeId> {
        let root = self.root(layout);
        root.children(&self.tree.map).collect()
    }

    /// Reorders columns to match the given order.
    /// `sorted_columns` should contain the same NodeIds as `columns()` but in the desired order.
    pub fn reorder_columns(&mut self, layout: LayoutId, sorted_columns: Vec<NodeId>) {
        let root = self.root(layout);
        // Detach all columns and reattach in sorted order
        for col in &sorted_columns {
            col.detach(&mut self.tree).push_back(root);
        }
    }

    pub fn set_column_weight(&mut self, node: NodeId, weight: f32) {
        self.tree.data.size.set_weight(node, weight, &self.tree.map);
    }

    /// Resets all node weights in the layout to 1.0, giving equal distribution.
    pub fn reset_weights(&mut self, layout: LayoutId) {
        let root = self.root(layout);
        for node in root.traverse_preorder(&self.tree.map).collect::<Vec<_>>() {
            if node != root {
                self.tree.data.size.set_weight(node, 1.0, &self.tree.map);
            }
        }
    }

    pub fn clone_layout(&mut self, layout: LayoutId) -> LayoutId {
        let source_root = self.layout_roots[layout].id();
        let cloned = source_root.deep_copy(&mut self.tree).make_root("layout_root");
        let cloned_root = cloned.id();
        let dest_layout = self.layout_roots.insert(cloned);
        self.layout_kinds.insert(dest_layout, self.layout_kind(layout));
        self.print_tree(layout);
        for (src, dest) in iter::zip(
            source_root.traverse_preorder(&self.tree.map),
            cloned_root.traverse_preorder(&self.tree.map),
        ) {
            self.tree.data.dispatch_event(&self.tree.map, TreeEvent::Copied { src, dest });
        }
        dest_layout
    }

    pub fn add_window_under(&mut self, layout: LayoutId, parent: NodeId, wid: WindowId) -> NodeId {
        if let Some(node) = self.unstash_window(layout, wid) {
            return node;
        }
        debug_assert_eq!(
            parent.ancestors(self.map()).last(),
            Some(self.root(layout)),
            "parent must belong to the given layout"
        );
        let node = self.tree.mk_node().push_back(parent);
        self.tree.data.window.set_window(node, wid);
        node
    }

    pub fn add_window_after(&mut self, layout: LayoutId, sibling: NodeId, wid: WindowId) -> NodeId {
        if let Some(node) = self.unstash_window(layout, wid) {
            return node;
        }
        if sibling.parent(self.map()).is_none() {
            // Don't attempt to add next to the root node.
            return self.add_window_under(layout, sibling, wid);
        }
        let node = self.tree.mk_node().insert_after(sibling);
        self.tree.data.window.set_window(node, wid);
        node
    }

    /// Moves `moving_node` to be a sibling after `sibling`.
    ///
    /// `sibling` may be in a different layout, in which case `moving_node` and
    /// its subtree change layouts.
    pub fn move_node_after(&mut self, sibling: NodeId, moving_node: NodeId) {
        let map = &self.tree.map;
        let Some(old_parent) = moving_node.parent(map) else {
            return;
        };
        let is_selection =
            self.tree.data.selection.local_selection(map, old_parent) == Some(moving_node);
        if sibling.parent(self.map()).is_none() {
            // Don't attempt to add next to the root node.
            moving_node.detach(&mut self.tree).push_back(sibling);
        } else {
            moving_node.detach(&mut self.tree).insert_after(sibling);
        }
        if is_selection {
            for node in moving_node.ancestors(&self.tree.map).take_while(|&a| a != old_parent) {
                self.tree.data.selection.select_locally(&self.tree.map, node);
            }
        }
    }

    pub fn move_node_before(&mut self, sibling: NodeId, moving_node: NodeId) {
        let map = &self.tree.map;
        let Some(old_parent) = moving_node.parent(map) else {
            return;
        };
        let is_selection =
            self.tree.data.selection.local_selection(map, old_parent) == Some(moving_node);
        if sibling.parent(self.map()).is_none() {
            // Don't attempt to add before the root node.
            moving_node.detach(&mut self.tree).push_front(sibling);
        } else {
            moving_node.detach(&mut self.tree).insert_before(sibling);
        }
        if is_selection {
            for node in moving_node.ancestors(&self.tree.map).take_while(|&a| a != old_parent) {
                self.tree.data.selection.select_locally(&self.tree.map, node);
            }
        }
    }

    #[allow(dead_code)]
    pub fn add_windows_if_missing(
        &mut self,
        layout: LayoutId,
        parent: NodeId,
        wids: impl Iterator<Item = WindowId>,
    ) {
        self.tree.map.reserve(wids.size_hint().1.unwrap_or(0));
        self.tree.data.window.set_capacity(self.tree.map.capacity());
        for wid in wids {
            if self.window_node(layout, wid).is_none() {
                self.add_window_under(layout, parent, wid);
            }
        }
    }

    pub fn remove_window_from(&mut self, layout: LayoutId, wid: WindowId) {
        self.drop_stashes(|other, other_wid| other == layout && other_wid == wid);
        if let Some(node) = self.window_node(layout, wid) {
            node.detach(&mut self.tree).remove();
        }
    }

    /// Transfers a native tab group's existing leaves to its selected window.
    /// Each layout keeps the old leaf's position, weight, and selection.
    pub fn replace_window(&mut self, old: WindowId, selected: WindowId) {
        if old == selected {
            return;
        }
        let layouts: Vec<_> = self
            .layout_roots
            .keys()
            .filter(|&layout| {
                self.window_node(layout, old).is_some() || self.window_is_stashed(layout, old)
            })
            .collect();
        for layout in layouts {
            self.remove_window_from(layout, selected);
            if let Some(stashed) = self.stashed.remove(&(layout, old)) {
                self.stashed.insert((layout, selected), stashed);
            }
        }
        let nodes: Vec<_> = self.tree.data.window.nodes_for(old).collect();
        for node in nodes {
            self.tree.data.window.replace_at(node, selected);
        }
        if let Some(share) = self.size_lock(old) {
            self.set_size_lock(selected, share);
        } else {
            self.clear_size_lock(selected);
        }
    }

    pub fn remove_window(&mut self, wid: WindowId) {
        self.drop_stashes(|_, other| other == wid);
        for node in self.tree.data.window.take_nodes_for(wid) {
            node.detach(&mut self.tree).remove();
        }
    }

    pub fn remove_windows_for_app(&mut self, pid: pid_t) {
        self.drop_stashes(|_, wid| wid.pid == pid);
        for (_, node) in self.tree.data.window.take_nodes_for_app(pid) {
            node.detach(&mut self.tree).remove();
        }
    }

    pub fn retain_apps(&mut self, filter: impl Fn(pid_t) -> bool) {
        let remove_pids =
            self.tree.data.window.pids().filter(|&pid| !filter(pid)).collect::<Vec<pid_t>>();
        for pid in remove_pids {
            self.remove_windows_for_app(pid);
        }
    }

    /// Adds and removes windows so that the set of windows in a space is exactly `wids`.
    ///
    /// For now, new windows are added directly to the root node.
    pub fn set_windows_for_app(
        &mut self,
        layout: LayoutId,
        app: pid_t,
        mut desired: Vec<WindowId>,
    ) {
        let root = self.root(layout);
        let mut current = root
            .traverse_postorder(self.map())
            .filter_map(|node| self.window_at(node).map(|wid| (wid, node)))
            .filter(|(wid, _)| wid.pid == app)
            .collect::<Vec<_>>();
        desired.sort_unstable();
        current.sort_unstable();
        debug_assert!(desired.iter().all(|wid| wid.pid == app));

        let mut desired = desired.into_iter().peekable();
        let mut current = current.into_iter().peekable();
        loop {
            match (desired.peek(), current.peek()) {
                (Some(des), Some((cur, _))) if des == cur => {
                    desired.next();
                    current.next();
                }
                (Some(des), None) => {
                    self.add_window_under(layout, root, *des);
                    desired.next();
                }
                (Some(des), Some((cur, _))) if des < cur => {
                    self.add_window_under(layout, root, *des);
                    desired.next();
                }
                (_, Some((_, node))) => {
                    node.detach(&mut self.tree).remove();
                    current.next();
                }
                (None, None) => break,
            }
        }
    }

    pub fn window_node(&self, layout: LayoutId, wid: WindowId) -> Option<NodeId> {
        let root = self.root(layout);
        self.tree
            .data
            .window
            .nodes_for(wid)
            .find(|node| node.ancestors(self.map()).last() == Some(root))
    }

    /// Takes the window's node out of `layout`, so that the layout closes its
    /// gap, but keeps the node and its place. The window returns there when
    /// it is added to the layout again. Returns false when the layout has no
    /// node for the window.
    ///
    /// When the node is the only child of its parent, the parent is taken
    /// out too, and so on up to the root, so that the window returns inside
    /// the same arrangement. The layouts of a whole Space are stashed when a
    /// window moves to another screen (R8, R9).
    pub fn stash_window(&mut self, layout: LayoutId, wid: WindowId) -> bool {
        let Some(node) = self.window_node(layout, wid) else {
            return false;
        };
        let root = self.root(layout);
        // The highest ancestor that would be culled with the node.
        let mut top = node;
        while let Some(parent) = top.parent(self.map()) {
            if parent == root
                || parent.first_child(self.map()) != Some(top)
                || parent.last_child(self.map()) != Some(top)
            {
                break;
            }
            top = parent;
        }
        let parent = top.parent(self.map());
        let prev = top.prev_sibling(self.map());
        let next = top.next_sibling(self.map());
        let Some(parent) = parent else { return false };
        let weight = self.tree.data.size.weight(top);
        let node = OwnedNode::stash(&mut self.tree, top, "stashed window");
        self.stashed.insert(
            (layout, wid),
            StashedNode {
                node,
                weight,
                parent,
                prev,
                next,
            },
        );
        true
    }

    /// Whether the window holds a place in `layout` while it is away.
    pub fn window_is_stashed(&self, layout: LayoutId, wid: WindowId) -> bool {
        self.stashed.contains_key(&(layout, wid))
    }

    /// Puts the window's stashed node back into `layout`, at the place it
    /// held when it left, as far as that place still exists. Does nothing
    /// when the window has no stash in the layout.
    pub fn unstash_window(&mut self, layout: LayoutId, wid: WindowId) -> Option<NodeId> {
        let stashed = self.stashed.remove(&(layout, wid))?;
        let root = self.root(layout);
        let alive = |node: NodeId| {
            self.tree.map.contains(node) && node.ancestors(self.map()).last() == Some(root)
        };
        let position = if let Some(prev) = stashed.prev.filter(|&prev| alive(prev)) {
            StashPosition::After(prev)
        } else if let Some(next) = stashed.next.filter(|&next| alive(next)) {
            StashPosition::Before(next)
        } else if alive(stashed.parent) {
            StashPosition::LastChild(stashed.parent)
        } else {
            StashPosition::LastChild(root)
        };
        let mut node = stashed.node;
        let id = node.reattach(&mut self.tree, position);
        self.tree.data.size.set_weight(id, stashed.weight, &self.tree.map);
        Some(id)
    }

    /// Drops the stashes that `filter` names, removing their nodes for good.
    fn drop_stashes(&mut self, mut filter: impl FnMut(LayoutId, WindowId) -> bool) {
        let keys: Vec<(LayoutId, WindowId)> = self
            .stashed
            .keys()
            .copied()
            .filter(|&(layout, wid)| filter(layout, wid))
            .collect();
        for key in keys {
            if let Some(mut stashed) = self.stashed.remove(&key) {
                stashed.node.remove(&mut self.tree);
            }
        }
    }

    /// Whether the window is in any layout.
    pub fn has_window(&self, wid: WindowId) -> bool {
        self.tree.data.window.nodes_for(wid).next().is_some()
    }

    pub fn window_at(&self, node: NodeId) -> Option<WindowId> {
        self.tree.data.window.at(node)
    }

    /// The smallest size observed for the window, if any constraint is known.
    pub fn window_min_size(&self, wid: WindowId) -> Option<CGSize> {
        self.tree.data.window.min_size(wid)
    }

    /// Records a lower bound on the window's size, merging per axis.
    pub fn note_window_min_size(&mut self, wid: WindowId, min_size: CGSize) {
        self.tree.data.window.note_min_size(wid, min_size);
    }

    /// Lowers the recorded minimum for the window to at most `size` per axis.
    pub fn relax_window_min_size(&mut self, wid: WindowId, size: CGSize) {
        self.tree.data.window.relax_min_size(wid, size);
    }

    /// Clears the recorded minimum size for the window.
    ///
    /// Called when a window is rearranged so it can adapt to its new layout.
    pub fn clear_window_min_size(&mut self, wid: WindowId) {
        self.tree.data.window.clear_min_size(wid);
    }

    #[allow(dead_code)]
    pub fn add_container(&mut self, parent: NodeId, kind: ContainerKind) -> NodeId {
        let node = self.tree.mk_node().push_back(parent);
        self.tree.data.size.set_kind(node, kind);
        node
    }

    pub fn select(&mut self, selection: NodeId) {
        self.tree.data.selection.select(&self.tree.map, selection)
    }

    pub fn selection(&self, layout: LayoutId) -> NodeId {
        self.tree.data.selection.current_selection(self.root(layout))
    }

    pub fn ascend_selection(&mut self, layout: LayoutId) -> bool {
        if let Some(parent) = self.selection(layout).parent(self.map()) {
            self.select(parent);
            return true;
        }
        false
    }

    pub fn descend_selection(&mut self, layout: LayoutId) -> bool {
        if let Some(child) =
            self.tree.data.selection.last_selection(self.map(), self.selection(layout))
        {
            self.select(child);
            return true;
        }
        false
    }

    pub fn set_fullscreen(&mut self, node: NodeId, is_fullscreen: bool) {
        self.tree.data.size.set_fullscreen(node, is_fullscreen)
    }

    pub fn is_fullscreen(&mut self, node: NodeId) -> bool {
        self.tree.data.size.is_fullscreen(node)
    }

    pub fn toggle_fullscreen(&mut self, node: NodeId) -> bool {
        let fullscreen = !self.is_fullscreen(node);
        self.set_fullscreen(node, fullscreen);
        fullscreen
    }

    pub fn set_size_lock(&mut self, wid: WindowId, share: f64) {
        self.tree.data.size.set_lock(wid, share);
    }

    pub fn clear_size_lock(&mut self, wid: WindowId) -> bool {
        self.tree.data.size.clear_lock(wid)
    }

    pub fn clear_size_locks(&mut self) {
        self.tree.data.size.clear_locks();
    }

    pub fn retain_size_locks(&mut self, filter: impl FnMut(WindowId) -> bool) {
        self.tree.data.size.retain_locks(filter);
    }

    pub fn size_lock(&self, wid: WindowId) -> Option<f64> {
        self.tree.data.size.lock(wid)
    }

    /// Every node mapped to `wid`, across all layouts.
    pub fn nodes_for_window(&self, wid: WindowId) -> Vec<NodeId> {
        self.tree.data.window.nodes_for(wid).collect()
    }

    /// The node whose rectangle holds the estate for `node`.
    ///
    /// This is `node` itself unless it is inside a group, in which case it is
    /// the outermost group: groups show one rectangle for all their windows.
    pub fn estate_node(&self, node: NodeId) -> NodeId {
        let map = self.map();
        let mut estate = node;
        for ancestor in node.ancestors(map) {
            if self.tree.data.size.kind(ancestor).is_group() {
                estate = ancestor;
            }
        }
        estate
    }

    /// The size share lock that applies to the estate containing `node`.
    pub fn estate_size_lock(&self, node: NodeId) -> Option<f64> {
        let estate = self.estate_node(node);
        if let Some(wid) = self.window_at(estate) {
            return self.size_lock(wid);
        }
        estate
            .traverse_preorder(self.map())
            .filter_map(|n| self.window_at(n))
            .filter_map(|wid| self.size_lock(wid))
            .reduce(f64::max)
    }

    /// Removes any size share lock that applies to the estate containing
    /// `node`, returning whether one was removed.
    pub fn clear_estate_size_lock(&mut self, node: NodeId) -> bool {
        self.clear_estate_size_lock_impl(node, false)
    }

    /// Removes any size share lock, optionally baking the lock value into
    /// the stored weights so the estate maintains its current visual share.
    ///
    /// Use `bake = true` when the user is manually resizing, so the window
    /// doesn't jump back to its pre-lock size.
    pub fn clear_estate_size_lock_and_bake(&mut self, node: NodeId) -> bool {
        self.clear_estate_size_lock_impl(node, true)
    }

    fn clear_estate_size_lock_impl(&mut self, node: NodeId, bake: bool) -> bool {
        let estate = self.estate_node(node);

        // Get the lock value before clearing, so we can bake it into the weights.
        if bake {
            if let Some(lock_share) = self.estate_size_lock(node) {
                self.bake_estate_share(estate, lock_share);
            }
        }

        let wids = estate
            .traverse_preorder(self.map())
            .filter_map(|n| self.window_at(n))
            .collect::<Vec<_>>();
        let mut cleared = false;
        for wid in wids {
            cleared |= self.tree.data.size.clear_lock(wid);
        }
        cleared
    }

    /// Adjusts the stored weights so that `estate` has proportion `share` of
    /// its parent, redistributing weight from/to siblings proportionally.
    fn bake_estate_share(&mut self, estate: NodeId, share: f64) {
        let map = self.map();
        let Some(parent) = estate.parent(map) else {
            return;
        };

        let parent_total = self.tree.data.size.total(parent);
        if parent_total <= 0.0 {
            return;
        }

        let current_weight = f64::from(self.tree.data.size.weight(estate));
        let target_weight = share * parent_total;
        let delta = target_weight - current_weight;

        if delta.abs() < 1e-6 {
            return;
        }

        // Collect siblings and their weights (excluding the estate itself)
        let siblings: Vec<(NodeId, f64)> = parent
            .children(map)
            .filter(|&child| child != estate)
            .map(|child| (child, f64::from(self.tree.data.size.weight(child))))
            .collect();

        if siblings.is_empty() {
            // No siblings to redistribute from, just set the weight directly
            self.tree.data.size.set_weight(estate, target_weight as f32, &self.tree.map);
            return;
        }

        // Calculate total sibling weight for proportional redistribution
        let sibling_total: f64 = siblings.iter().map(|(_, w)| w).sum();

        if sibling_total <= 0.0 {
            return;
        }

        // Redistribute delta proportionally among siblings
        for (sibling, sibling_weight) in &siblings {
            let sibling_proportion = sibling_weight / sibling_total;
            let new_weight = (sibling_weight - delta * sibling_proportion).max(0.1);
            self.tree.data.size.set_weight(*sibling, new_weight as f32, &self.tree.map);
        }

        // Set the estate's new weight
        self.tree.data.size.set_weight(estate, target_weight as f32, &self.tree.map);
    }

    /// Removes the fullscreen flag from `node` and its ancestors, if set.
    pub fn clear_fullscreen_for(&mut self, node: NodeId) {
        let nodes = node.ancestors(self.map()).collect::<Vec<_>>();
        for node in nodes {
            if self.tree.data.size.is_fullscreen(node) {
                self.tree.data.size.set_fullscreen(node, false);
            }
        }
    }

    /// Whether the estate containing `node` has a size share lock.
    pub fn estate_is_locked(&self, node: NodeId) -> bool {
        self.estate_size_lock(node).is_some()
    }

    /// The current share of the estate containing `node` within its parent,
    /// as a fraction from 0 to 1.
    pub fn estate_share(&self, _layout: LayoutId, node: NodeId) -> Option<f64> {
        let estate = self.estate_node(node);
        let parent = estate.parent(self.map())?;

        let parent_total = self.tree.data.size.total(parent);
        if parent_total <= 0.0 {
            return None;
        }

        let estate_weight = f64::from(self.tree.data.size.weight(estate));
        Some(estate_weight / parent_total)
    }

    /// The sum of the size share locks in `layout`, counting each estate once.
    pub fn locked_share_total(&self, layout: LayoutId) -> f64 {
        let root = self.root(layout);
        let mut estates = HashSet::new();
        let mut total = 0.0;
        for node in root.traverse_preorder(self.map()) {
            if self.window_at(node).is_none() {
                continue;
            }
            let estate = self.estate_node(node);
            if estates.insert(estate)
                && let Some(share) = self.estate_size_lock(estate)
            {
                total += share;
            }
        }
        total
    }

    /// The target shares for `root`, when size share locks apply to it.
    fn layout_masses(&self, root: NodeId) -> Option<Masses> {
        if self.is_scroll_root(root) {
            return None;
        }
        self.tree.data.size.masses(&self.tree.map, &self.tree.data.window, root)
    }

    pub fn calculate_layout(
        &self,
        layout: LayoutId,
        frame: CGRect,
        config: &Config,
    ) -> Vec<(WindowId, CGRect)> {
        self.tree.data.size.get_sizes(
            &self.tree.map,
            &self.tree.data.window,
            &self.tree.data.selection,
            config,
            self.root(layout),
            frame,
            self.is_scroll_layout(layout),
        )
    }

    pub fn calculate_layout_and_groups(
        &self,
        layout: LayoutId,
        frame: CGRect,
        config: &Config,
    ) -> (Vec<(WindowId, CGRect)>, Vec<super::GroupBarInfo>) {
        self.tree.data.size.get_sizes_and_groups(
            &self.tree.map,
            &self.tree.data.window,
            &self.tree.data.selection,
            config,
            self.root(layout),
            frame,
            self.is_scroll_layout(layout),
        )
    }

    pub fn traverse(&self, from: NodeId, direction: Direction) -> Option<NodeId> {
        let map = &self.tree.map;
        let node =
            // Keep going up...
            from.ancestors(map)
            // ...until we can move in the desired direction, then move.
            .flat_map(|n| self.move_over(n, direction)).next();
        // Descend as far down as we can go, keeping close to the direction we're
        // moving from.
        iter::successors(node, |&node| {
            if self.tree.data.size.kind(node).orientation() == direction.orientation() {
                match direction {
                    Direction::Up | Direction::Left => node.last_child(map),
                    Direction::Down | Direction::Right => node.first_child(map),
                }
            } else {
                self.tree.data.selection.local_selection(map, node).or(node.first_child(map))
            }
        })
        .last()
    }

    pub fn focus_next(&self, layout: LayoutId, current: NodeId) -> Option<NodeId> {
        let windows = self.windows_in_order(layout);
        if windows.is_empty() {
            return None;
        }
        let pos = windows.iter().position(|&n| n == current);
        let next_pos = match pos {
            Some(p) => (p + 1) % windows.len(),
            None => 0,
        };
        Some(windows[next_pos])
    }

    pub fn focus_prev(&self, layout: LayoutId, current: NodeId) -> Option<NodeId> {
        let windows = self.windows_in_order(layout);
        if windows.is_empty() {
            return None;
        }
        let pos = windows.iter().position(|&n| n == current);
        let prev_pos = match pos {
            Some(p) if p == 0 => windows.len() - 1,
            Some(p) => p - 1,
            None => windows.len() - 1,
        };
        Some(windows[prev_pos])
    }

    fn windows_in_order(&self, layout: LayoutId) -> Vec<NodeId> {
        self.root(layout)
            .traverse_preorder(self.map())
            .filter(|&node| self.window_at(node).is_some())
            .collect()
    }

    pub fn traverse_scroll_wrapping(
        &self,
        layout: LayoutId,
        from: NodeId,
        direction: Direction,
    ) -> Option<NodeId> {
        let root = self.root(layout);
        let columns: Vec<NodeId> = root.children(&self.tree.map).collect();
        let len = columns.len();
        if len == 0 {
            return None;
        }
        let current_col = from
            .ancestors(&self.tree.map)
            .find(|&n| n.parent(&self.tree.map) == Some(root))?;
        let idx = columns.iter().position(|&c| c == current_col)?;
        let step: isize = match direction {
            Direction::Left => -1,
            Direction::Right => 1,
            _ => return self.traverse(from, direction),
        };
        let new_idx = (idx as isize + step).rem_euclid(len as isize) as usize;
        if new_idx == idx {
            return None;
        }
        let target_col = columns[new_idx];
        let mut node = target_col;
        while let Some(child) = self
            .tree
            .data
            .selection
            .local_selection(&self.tree.map, node)
            .or(node.first_child(&self.tree.map))
        {
            node = child;
        }
        Some(node)
    }

    pub fn select_returning_surfaced_windows(&mut self, selection: NodeId) -> Vec<WindowId> {
        let map = &self.tree.map;
        let mut highest_revealed = selection;
        for (node, parent) in selection.ancestors_with_parent(map) {
            let Some(parent) = parent else { break };
            if self.tree.data.selection.select_locally(map, node) {
                if self.container_kind(parent).is_group() {
                    highest_revealed = node;
                }
            }
        }
        self.visible_windows_under(highest_revealed)
    }

    pub fn visible_windows_under(&self, node: NodeId) -> Vec<WindowId> {
        let mut stack = vec![node];
        let mut windows = vec![];
        while let Some(node) = stack.pop() {
            if self.container_kind(node).is_group() {
                stack.extend(self.tree.data.selection.local_selection(self.map(), node));
            } else {
                stack.extend(node.children(self.map()));
            }
            windows.extend(self.window_at(node));
        }
        windows
    }

    pub fn is_visible(&self, node: NodeId) -> bool {
        for (node, parent) in node.ancestors_with_parent(self.map()) {
            let Some(parent) = parent else { break };
            if self.container_kind(parent).is_group()
                && self.tree.data.selection.local_selection(self.map(), parent) != Some(node)
            {
                return false;
            }
        }
        true
    }

    fn move_over(&self, from: NodeId, direction: Direction) -> Option<NodeId> {
        let Some(parent) = from.parent(&self.tree.map) else {
            return None;
        };
        if self.tree.data.size.kind(parent).orientation() == direction.orientation() {
            match direction {
                Direction::Up | Direction::Left => from.prev_sibling(&self.tree.map),
                Direction::Down | Direction::Right => from.next_sibling(&self.tree.map),
            }
        } else {
            None
        }
    }

    pub fn move_node(
        &mut self,
        layout: LayoutId,
        moving_node: NodeId,
        direction: Direction,
    ) -> bool {
        let map = &self.tree.map;
        let Some(old_parent) = moving_node.parent(map) else {
            return false;
        };
        let is_selection =
            self.tree.data.selection.local_selection(map, old_parent) == Some(moving_node);
        let moved = self.move_node_inner(layout, moving_node, direction);
        if moved && is_selection {
            for node in moving_node.ancestors(&self.tree.map).take_while(|&a| a != old_parent) {
                self.tree.data.selection.select_locally(&self.tree.map, node);
            }
        }
        moved
    }

    fn move_node_inner(
        &mut self,
        layout: LayoutId,
        moving_node: NodeId,
        direction: Direction,
    ) -> bool {
        /// Where to insert the node, along the direction we're moving.
        enum Destination {
            Ahead(NodeId),
            Behind(NodeId),
        }
        let map = &self.tree.map;
        let destination;
        if let Some(sibling) = self.move_over(moving_node, direction) {
            // Traverse down the sibling until we hit the next node with
            // the same orientation we're moving in.
            let mut node = sibling;
            let target = loop {
                let Some(next) =
                    self.tree.data.selection.local_selection(map, node).or(node.first_child(map))
                else {
                    break node;
                };
                if self.tree.data.size.kind(node).orientation() == direction.orientation() {
                    break next;
                }
                node = next;
            };
            if target == sibling {
                // Our sibling is a leaf; we're switching places.
                destination = Destination::Ahead(sibling);
            } else {
                // The target is our new sibling. We have already moved laterally,
                // so don't do that here.
                destination = Destination::Behind(target);
            }
        } else {
            // Traverse up the tree until we can move in the desired direction.
            let target = moving_node
                .ancestors_with_parent(&self.tree.map)
                .skip(1) // We already tried moving at the current level.
                .skip_while(|(_node, parent)| {
                    parent
                        .map(|p| self.container_kind(p).orientation() != direction.orientation())
                        // If we get all the way to the root, give up and skip it too.
                        .unwrap_or(true)
                })
                .next();
            if let Some((target, _parent)) = target {
                // The target is our new sibling. We haven't moved laterally yet, so do that here.
                destination = Destination::Ahead(target);
            } else {
                // We went all the way to the root and couldn't move in the
                // desired direction, so we'll make a new container level above it.
                let old_root = moving_node.ancestors(map).last().unwrap();
                if self.tree.data.size.kind(old_root).orientation() == direction.orientation() {
                    // Arguably it's not that useful to do this in the same direction as the root,
                    // so let's stop here. (This will become a screen move if there are
                    // multiple screens.)
                    return false;
                }
                self.nest_in_container(
                    layout,
                    old_root,
                    ContainerKind::from(direction.orientation()),
                );
                destination = Destination::Ahead(old_root);
            }
        }
        match (destination, direction) {
            (Destination::Ahead(target), Direction::Right | Direction::Down) => {
                moving_node.detach(&mut self.tree).insert_after(target);
            }
            (Destination::Behind(target), Direction::Right | Direction::Down) => {
                moving_node.detach(&mut self.tree).insert_before(target);
            }
            (Destination::Ahead(target), Direction::Left | Direction::Up) => {
                moving_node.detach(&mut self.tree).insert_before(target);
            }
            (Destination::Behind(target), Direction::Left | Direction::Up) => {
                moving_node.detach(&mut self.tree).insert_after(target);
            }
        }
        true
    }

    pub fn map(&self) -> &NodeMap {
        &self.tree.map
    }

    pub fn node_exists(&self, node: NodeId) -> bool {
        self.tree.map.contains(node)
    }

    pub fn container_kind(&self, node: NodeId) -> ContainerKind {
        self.tree.data.size.kind(node)
    }

    pub fn proportion(&self, node: NodeId) -> Option<f64> {
        self.tree.data.size.proportion(&self.tree.map, node)
    }

    pub fn last_ungrouped_container_kind(&self, node: NodeId) -> ContainerKind {
        self.tree.data.size.last_ungrouped_kind(node)
    }

    pub fn set_container_kind(&mut self, node: NodeId, kind: ContainerKind) {
        self.tree.data.size.set_kind(node, kind);
    }

    pub fn nest_in_container(
        &mut self,
        layout: LayoutId,
        node: NodeId,
        kind: ContainerKind,
    ) -> NodeId {
        let old_parent = node.parent(&self.tree.map);
        let parent = if node.prev_sibling(&self.tree.map).is_none()
            && node.next_sibling(&self.tree.map).is_none()
            && old_parent.is_some()
        {
            old_parent.unwrap()
        } else {
            let new_parent = if let Some(old_parent) = old_parent {
                let is_selection =
                    self.tree.data.selection.local_selection(self.map(), old_parent) == Some(node);
                let new_parent = self.tree.mk_node().insert_before(node);
                self.tree.data.size.assume_size_of(new_parent, node, &self.tree.map);
                node.detach(&mut self.tree).push_back(new_parent);
                if is_selection {
                    self.tree.data.selection.select_locally(&self.tree.map, new_parent);
                }
                new_parent
            } else {
                // New root.
                let layout_root = self.layout_roots.get_mut(layout).unwrap();
                layout_root.replace(self.tree.mk_node()).push_back(layout_root.id());
                layout_root.id()
            };
            self.tree.data.selection.select_locally(&self.tree.map, node);
            new_parent
        };
        self.tree.data.size.set_kind(parent, kind);
        parent
    }

    pub fn swap_windows(&mut self, node_a: NodeId, node_b: NodeId) {
        self.tree.data.window.swap_windows(node_a, node_b);
        // Swap weights so window sizes follow the windows, not the tree positions.
        self.tree.data.size.swap_weights(node_a, node_b);
    }

    /// Picks the ancestor of `node` (possibly `node` itself) to resize, along
    /// with the sibling it exchanges space with.
    ///
    /// A size locked estate can't give up any space, so its boundaries are
    /// inert: there is no sibling to exchange with.
    fn resize_target(&self, node: NodeId, direction: Direction) -> Option<(NodeId, NodeId)> {
        let can_resize = |&node: &NodeId| -> bool {
            let Some(parent) = node.parent(&self.tree.map) else {
                return false;
            };
            let Some(sibling) = self.move_over(node, direction) else {
                return false;
            };
            !self.tree.data.size.kind(parent).is_group() && !self.estate_is_locked(sibling)
        };
        let resizing_node = node.ancestors(&self.tree.map).filter(can_resize).next()?;
        let sibling = self.move_over(resizing_node, direction).unwrap();
        Some((resizing_node, sibling))
    }

    /// The number of pixels one unit of `parent`'s total weight is worth.
    ///
    /// Returns `None` if `parent` is not laid out on `screen`.
    fn pixels_per_weight(&self, parent: NodeId, screen: CGRect, config: &Config) -> Option<f64> {
        let root = parent.ancestors(&self.tree.map).last().unwrap();
        let parent_rect = self.tree.data.size.get_node_rect(
            &self.tree.map,
            &self.tree.data.window,
            &self.tree.data.selection,
            config,
            root,
            screen,
            self.is_scroll_root(root),
            parent,
        )?;
        Some(self.tree.data.size.pixels_per_weight(
            &self.tree.map,
            parent,
            parent_rect,
            config,
            self.is_scroll_root(parent),
        ))
    }

    /// Resizes `node` by `delta` pixels along `direction`.
    ///
    /// This is not the same as [`Self::resize`] with `delta` as a fraction of
    /// the screen: gaps take up screen space that is not distributed by weight,
    /// so a fraction of the screen is worth fewer pixels in the layout.
    ///
    /// The resulting frame can still land a pixel away from the requested one,
    /// since each child's frame is rounded independently and positions follow
    /// the rounded edge of the previous sibling.
    pub fn resize_by_pixels(
        &mut self,
        node: NodeId,
        delta: f64,
        direction: Direction,
        screen: CGRect,
        config: &Config,
    ) -> bool {
        let Some((resizing_node, sibling)) = self.resize_target(node, direction) else {
            return false;
        };
        let parent = resizing_node.parent(&self.tree.map).unwrap();
        let Some(px_per_weight) = self.pixels_per_weight(parent, screen, config) else {
            return false;
        };
        if px_per_weight <= 0.0 {
            return false;
        }
        let mut delta_weight = delta / px_per_weight;
        // `pixels_per_weight` is pixels per unit of *stored* weight, while
        // `solve_sizes` sees masses. When locks apply, a weight change is worth
        // more or fewer pixels depending on how much of the parent is locked
        // and on which unlocked children absorb the exchange. Scale the delta
        // so the window tracks the pointer. The sibling is never a locked
        // estate, since `resize_target` treats those boundaries as inert.
        if let Some(masses) = self.layout_masses(parent.ancestors(&self.tree.map).last().unwrap()) {
            let total = masses.total(parent);
            let unlocked_mass = total - masses.locked(parent);
            let stored_total = self.tree.data.size.total(parent);
            let pool_weight: f64 = parent
                .children(&self.tree.map)
                .filter(|&child| masses.total(child) > masses.locked(child))
                .map(|child| f64::from(self.tree.data.size.weight(child)))
                .sum();
            if unlocked_mass > 0.0 && pool_weight > 0.0 && stored_total > 0.0 {
                delta_weight *= total * pool_weight / (unlocked_mass * stored_total);
            }
        }
        self.apply_resize(resizing_node, sibling, parent, delta_weight);
        true
    }

    pub fn resize(&mut self, node: NodeId, screen_ratio: f64, direction: Direction) -> bool {
        let Some((resizing_node, sibling)) = self.resize_target(node, direction) else {
            return false;
        };

        // Compute the share of resizing_node's parent that needs to be taken
        // from the sibling.
        let exchange_rate = resizing_node.ancestors(&self.tree.map).skip(1).fold(1.0, |r, node| {
            match node.parent(&self.tree.map) {
                Some(parent)
                    if self.tree.data.size.kind(parent).orientation()
                        == direction.orientation()
                        && !self.tree.data.size.kind(parent).is_group() =>
                {
                    r * self.tree.data.size.proportion(&self.tree.map, node).unwrap()
                }
                _ => r,
            }
        });
        let parent = resizing_node.parent(&self.tree.map).unwrap();
        let parent_total = self.tree.data.size.total(parent);
        let local_ratio = if self.is_scroll_root(parent) {
            screen_ratio / exchange_rate
        } else {
            screen_ratio * parent_total / exchange_rate
        };
        self.apply_resize(resizing_node, sibling, parent, local_ratio);

        true
    }

    /// Moves `delta_weight` units of `parent`'s total weight to
    /// `resizing_node`, taking it from `sibling`.
    fn apply_resize(
        &mut self,
        resizing_node: NodeId,
        sibling: NodeId,
        parent: NodeId,
        delta_weight: f64,
    ) {
        if self.is_scroll_root(parent) {
            let current_weight = self.tree.data.size.weight(resizing_node);
            self.tree.data.size.set_weight(
                resizing_node,
                current_weight + delta_weight as f32,
                &self.tree.map,
            );
        } else {
            self.tree.data.size.take_share(
                &self.tree.map,
                resizing_node,
                sibling,
                delta_weight as f32,
            );
        }
    }

    /// Call this during a user resize to have the model respond appropriately.
    ///
    /// Only two edges are allowed to change at a time.
    pub fn set_frame_from_resize(
        &mut self,
        node: NodeId,
        old_frame: CGRect,
        new_frame: CGRect,
        screen: CGRect,
        config: &Config,
    ) {
        let mut check_or_resize = |resize: bool| {
            let mut count = 0;
            let mut first_direction: Option<Direction> = None;
            let mut good = true;
            let mut check_and_resize = |direction: Direction, delta| {
                if delta != 0.0 {
                    count += 1;
                    if count > 2 {
                        good = false;
                    }
                    if let Some(first) = first_direction {
                        if first.orientation() == direction.orientation() {
                            good = false;
                        }
                    } else {
                        first_direction = Some(direction);
                    }
                    if resize {
                        self.resize_by_pixels(node, delta, direction, screen, config);
                    }
                }
            };
            check_and_resize(Direction::Left, old_frame.min().x - new_frame.min().x);
            check_and_resize(Direction::Right, new_frame.max().x - old_frame.max().x);
            check_and_resize(Direction::Up, old_frame.min().y - new_frame.min().y);
            check_and_resize(Direction::Down, new_frame.max().y - old_frame.max().y);
            good
        };
        if !check_or_resize(false) {
            // This function doesn't work correctly for anything other than changing one edge or corner at a time.
            warn!(
                "Only resizing in 2 directions is supported, but was asked \
                to resize from {old_frame:?} to {new_frame:?}"
            );
            return;
        }
        check_or_resize(true);
    }

    pub fn print_tree(&self, layout: LayoutId) {
        print!("{}", self.draw_tree(layout))
    }

    pub fn draw_tree(&self, layout: LayoutId) -> String {
        let tree = self.get_ascii_tree(self.root(layout));
        let mut out = String::new();
        ascii_tree::write_tree(&mut out, &tree).unwrap();
        out
    }

    fn get_ascii_tree(&self, node: NodeId) -> ascii_tree::Tree {
        let status = match node.parent(&self.tree.map) {
            None => "", // Root
            Some(parent)
                if self.tree.data.selection.local_selection(&self.tree.map, parent)
                    == Some(node) =>
            {
                "☒ "
            }
            _ => "☐ ",
        };
        let desc = format!("{status}{node:?}",);
        let desc = match self.window_at(node) {
            Some(wid) => format!("{desc} {wid:?} {}", self.tree.data.size.debug(node, false)),
            None => format!("{desc} {}", self.tree.data.size.debug(node, true)),
        };
        let children: Vec<_> =
            node.children(&self.tree.map).map(|c| self.get_ascii_tree(c)).collect();
        if children.is_empty() {
            ascii_tree::Tree::Leaf(vec![desc])
        } else {
            ascii_tree::Tree::Node(desc, children)
        }
    }
}

impl Drop for LayoutTree {
    fn drop(&mut self) {
        for (_, node) in self.layout_roots.drain() {
            // It's okay to skip removing these, since we're dropping the map too.
            mem::forget(node);
        }
        for (_, stashed) in self.stashed.drain() {
            // The stashed nodes go with the map too.
            mem::forget(stashed.node);
        }
    }
}

/// The components of our data model own slices of information attached to every
/// node.
#[derive(Default, Serialize, Deserialize)]
struct Components {
    selection: Selection,
    #[serde(alias = "layout")]
    size: Size,
    window: Window,
}

#[derive(Copy, Clone)]
pub(super) enum TreeEvent {
    /// A node was added to the forest.
    AddedToForest(NodeId),
    /// A node was added to its parent. Note that the node may have existed in
    /// the tree previously under a different parent.
    AddedToParent(NodeId),
    /// A node has been copied from one tree to another.
    ///
    /// The destination node will have the same number of parents, siblings,
    /// and children as the source. No other events will fire on this node
    /// until the tree structure changes.
    Copied { src: NodeId, dest: NodeId },
    /// A node will be removed from its parent.
    RemovingFromParent(NodeId),
    /// A node was removed from the forest.
    RemovedFromForest(NodeId),
}

impl Components {
    fn dispatch_event(&mut self, map: &NodeMap, event: TreeEvent) {
        self.selection.handle_event(map, event);
        self.size.handle_event(map, event);
        self.window.handle_event(map, event);
    }
}

impl tree::Observer for Components {
    fn added_to_forest(&mut self, map: &NodeMap, node: NodeId) {
        self.dispatch_event(map, TreeEvent::AddedToForest(node))
    }

    fn added_to_parent(&mut self, map: &NodeMap, node: NodeId) {
        self.dispatch_event(map, TreeEvent::AddedToParent(node))
    }

    fn removing_from_parent(&mut self, map: &NodeMap, node: NodeId) {
        self.dispatch_event(map, TreeEvent::RemovingFromParent(node))
    }

    fn removed_child(tree: &mut Tree<Self>, parent: NodeId) {
        // Decide whether to cull the parent node (which must be a container).
        if parent.parent(&tree.map).is_none() {
            // Don't cull the root node, which would require extra bookkeeping.
            return;
        }
        if parent.is_empty(&tree.map) {
            parent.detach(tree).remove();
        } else if parent.first_child(&tree.map) == parent.last_child(&tree.map) {
            // Promote the only remaining child of the parent node.
            let child = parent.first_child(&tree.map).unwrap();
            child
                .detach(tree)
                .insert_after(parent)
                .with(|child_id, tree| {
                    // Assume the size of the parent before culling it.
                    tree.data.size.assume_size_of(child_id, parent, &tree.map)
                })
                // Notify that the child was removed; this will cull the parent.
                .finish();
        }
    }

    fn removed_from_forest(&mut self, map: &NodeMap, node: NodeId) {
        self.dispatch_event(map, TreeEvent::RemovedFromForest(node))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use objc2_core_foundation::{CGPoint, CGSize};
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::actor::app::pid_t;
    use crate::model::LayoutTree;

    fn w(pid: pid_t, idx: u32) -> WindowId {
        WindowId::new(pid, idx)
    }

    #[test]
    fn native_tab_selection_preserves_positions_and_weights_in_saved_layouts() {
        let mut tree = LayoutTree::new();
        let first = tree.create_layout();
        let node = tree.add_window_under(first, tree.root(first), w(1, 1));
        tree.add_window_under(first, tree.root(first), w(2, 1));
        tree.resize(node, 0.2, Direction::Right);
        tree.select(node);
        let second = tree.clone_layout(first);
        let second_node = tree.window_node(second, w(1, 1)).unwrap();
        tree.resize(second_node, -0.1, Direction::Right);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1200., 800.));
        let config = Config::default();
        let expected: Vec<_> = [first, second]
            .into_iter()
            .map(|layout| {
                tree.calculate_layout(layout, screen, &config)
                    .into_iter()
                    .map(|(wid, frame)| (if wid == w(1, 1) { w(1, 2) } else { wid }, frame))
                    .collect::<Vec<_>>()
            })
            .collect();
        tree.replace_window(w(1, 1), w(1, 2));
        for (i, layout) in [first, second].into_iter().enumerate() {
            assert_frames_are(
                tree.calculate_layout(layout, screen, &config),
                expected[i].clone(),
            );
            assert_eq!(Some(w(1, 2)), tree.window_at(tree.selection(layout)));
        }
    }

    #[test]
    fn native_tab_selection_keeps_a_stashed_position_on_another_screen() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        tree.add_window_under(layout, root, w(2, 1));
        let node = tree.add_window_under(layout, root, w(1, 1));
        tree.add_window_under(layout, root, w(2, 2));
        tree.resize(node, 0.2, Direction::Right);
        tree.set_size_lock(w(1, 1), 0.4);
        let screen = CGRect::new(CGPoint::ZERO, CGSize::new(1200., 800.));
        let config = Config::default();
        let expected: Vec<_> = tree
            .calculate_layout(layout, screen, &config)
            .into_iter()
            .map(|(wid, frame)| (if wid == w(1, 1) { w(1, 2) } else { wid }, frame))
            .collect();
        assert!(tree.stash_window(layout, w(1, 1)));
        tree.add_window_under(layout, root, w(1, 2));
        tree.replace_window(w(1, 1), w(1, 2));
        assert!(!tree.window_is_stashed(layout, w(1, 1)));
        assert!(tree.window_is_stashed(layout, w(1, 2)));
        assert!(tree.window_node(layout, w(1, 2)).is_none());
        tree.unstash_window(layout, w(1, 2)).unwrap();
        assert_frames_are(tree.calculate_layout(layout, screen, &config), expected);
        assert_eq!(tree.nodes_for_window(w(1, 2)).len(), 1);
    }

    #[test]
    fn set_windows_for_app() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, w(1, 1));
        let a2 = tree.add_container(root, ContainerKind::Vertical);
        let b1 = tree.add_window_under(layout, a2, w(2, 1));
        let b2 = tree.add_window_after(layout, b1, w(2, 2));
        let b3 = tree.add_window_after(layout, b2, w(2, 3));
        let a3 = tree.add_window_under(layout, root, w(1, 3));

        let get_windows = |tree: &LayoutTree| {
            root.traverse_postorder(tree.map())
                .filter_map(|node| tree.window_at(node))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            [w(1, 1), w(2, 1), w(2, 2), w(2, 3), w(1, 3)],
            *get_windows(&tree)
        );

        tree.set_windows_for_app(layout, 2, vec![w(2, 1), w(2, 3)]);
        assert_eq!([w(1, 1), w(2, 1), w(2, 3), w(1, 3)], *get_windows(&tree));
        assert_eq!(Some(w(1, 1)), tree.window_at(a1));
        assert_eq!(Some(w(2, 1)), tree.window_at(b1));
        assert_eq!(None, tree.window_at(b2));
        assert_eq!(Some(w(2, 3)), tree.window_at(b3));
        assert_eq!(Some(w(1, 3)), tree.window_at(a3));

        tree.set_windows_for_app(layout, 2, vec![]);
        assert_eq!([w(1, 1), w(1, 3)], *get_windows(&tree));
        tree.set_windows_for_app(layout, 1, vec![]);
        assert!(get_windows(&tree).is_empty());

        tree.set_windows_for_app(layout, 2, vec![w(2, 1), w(2, 3)]);
        assert_eq!([w(2, 1), w(2, 3)], *get_windows(&tree));
        assert_eq!(None, tree.window_at(a1));
        assert_eq!(None, tree.window_at(b1));
        assert_eq!(None, tree.window_at(b2));
        assert_eq!(None, tree.window_at(b3));
        assert_eq!(None, tree.window_at(a3));
        assert_eq!(2, root.children(tree.map()).count());
    }

    #[test]
    fn move_node_after_across_layouts_updates_window_mapping() {
        let mut tree = LayoutTree::new();
        let layout1 = tree.create_layout();
        let layout2 = tree.create_layout();
        let root2 = tree.root(layout2);

        let moving = tree.add_window_under(layout1, tree.root(layout1), w(1, 1));
        let sibling = tree.add_window_under(layout2, root2, w(2, 1));

        // Initially the window resolves only under layout1.
        assert_eq!(Some(moving), tree.window_node(layout1, w(1, 1)));
        assert_eq!(None, tree.window_node(layout2, w(1, 1)));

        tree.move_node_after(sibling, moving);

        // The node is reparented into layout2's tree.
        assert_eq!(Some(root2), moving.parent(tree.map()));
        // The node <-> window mapping is unchanged.
        assert_eq!(Some(w(1, 1)), tree.window_at(moving));
        // window_node now resolves the moved node under layout2, not layout1,
        // because the layout is derived from the node's position.
        assert_eq!(None, tree.window_node(layout1, w(1, 1)));
        assert_eq!(Some(moving), tree.window_node(layout2, w(1, 1)));
    }

    #[test]
    fn move_node_after_across_layouts_moves_whole_subtree() {
        let mut tree = LayoutTree::new();
        let layout1 = tree.create_layout();
        let layout2 = tree.create_layout();
        let root2 = tree.root(layout2);

        // A container in layout1 holding two windows.
        let container = tree.add_container(tree.root(layout1), ContainerKind::Vertical);
        let c1 = tree.add_window_under(layout1, container, w(1, 1));
        let _c2 = tree.add_window_after(layout1, c1, w(1, 2));
        let sibling = tree.add_window_under(layout2, root2, w(2, 1));

        tree.move_node_after(sibling, container);

        // The container moved under layout2's root, taking its windows with it.
        assert_eq!(Some(root2), container.parent(tree.map()));
        // Every descendant window now resolves under layout2, not just the
        // moved node itself.
        for wid in [w(1, 1), w(1, 2)] {
            assert_eq!(None, tree.window_node(layout1, wid));
            assert!(tree.window_node(layout2, wid).is_some());
        }
    }

    #[test]
    fn focus_next_prev_with_wraparound() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let n1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let n2 = tree.add_window_under(layout, root, WindowId::new(1, 2));
        let n3 = tree.add_window_under(layout, root, WindowId::new(1, 3));

        // Test next
        assert_eq!(tree.focus_next(layout, n1), Some(n2));
        assert_eq!(tree.focus_next(layout, n2), Some(n3));
        assert_eq!(tree.focus_next(layout, n3), Some(n1)); // wraparound

        // Test prev
        assert_eq!(tree.focus_prev(layout, n3), Some(n2));
        assert_eq!(tree.focus_prev(layout, n2), Some(n1));
        assert_eq!(tree.focus_prev(layout, n1), Some(n3)); // wraparound

        // Test with container selection
        assert_eq!(tree.focus_next(layout, root), Some(n1));
        assert_eq!(tree.focus_prev(layout, root), Some(n3));
    }

    #[test]
    fn focus_next_prev_nested() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let n1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let c1 = tree.add_container(root, ContainerKind::Vertical);
        let n2 = tree.add_window_under(layout, c1, WindowId::new(1, 2));
        let n3 = tree.add_window_under(layout, c1, WindowId::new(1, 3));

        // Order should be n1, n2, n3 (pre-order)
        assert_eq!(tree.focus_next(layout, n1), Some(n2));
        assert_eq!(tree.focus_next(layout, n2), Some(n3));
        assert_eq!(tree.focus_next(layout, n3), Some(n1));

        assert_eq!(tree.focus_prev(layout, n1), Some(n3));
        assert_eq!(tree.focus_prev(layout, n3), Some(n2));
        assert_eq!(tree.focus_prev(layout, n2), Some(n1));
    }

    #[test]
    fn retain_apps() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);

        // Create a layout with windows from three different apps
        let a1 = tree.add_window_under(layout, root, w(1, 1));
        let a2 = tree.add_window_under(layout, root, w(1, 2));
        let b1 = tree.add_window_under(layout, root, w(2, 1));
        let b2 = tree.add_window_under(layout, root, w(2, 2));
        let c1 = tree.add_window_under(layout, root, w(3, 1));

        let get_windows = |tree: &LayoutTree| {
            root.traverse_postorder(tree.map())
                .filter_map(|node| tree.window_at(node))
                .collect::<Vec<_>>()
        };

        // Verify all windows are present
        assert_eq!(
            [w(1, 1), w(1, 2), w(2, 1), w(2, 2), w(3, 1)],
            *get_windows(&tree)
        );

        // Simulate app 2 being terminated - retain only apps 1 and 3
        tree.retain_apps(|pid| pid == 1 || pid == 3);

        // Verify windows from app 2 are gone, but apps 1 and 3 remain
        assert_eq!([w(1, 1), w(1, 2), w(3, 1)], *get_windows(&tree));
        assert_eq!(Some(w(1, 1)), tree.window_at(a1));
        assert_eq!(Some(w(1, 2)), tree.window_at(a2));
        assert_eq!(None, tree.window_at(b1));
        assert_eq!(None, tree.window_at(b2));
        assert_eq!(Some(w(3, 1)), tree.window_at(c1));

        // Simulate all apps being terminated
        tree.retain_apps(|_| false);

        // Verify all windows are gone
        assert!(get_windows(&tree).is_empty());
        assert_eq!(None, tree.window_at(a1));
        assert_eq!(None, tree.window_at(a2));
        assert_eq!(None, tree.window_at(c1));
    }

    #[test]
    fn traverse() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let a2 = tree.add_container(root, ContainerKind::Vertical);
        let b1 = tree.add_window_under(layout, a2, WindowId::new(2, 1));
        let b2 = tree.add_window_under(layout, a2, WindowId::new(2, 2));
        let b3 = tree.add_window_under(layout, a2, WindowId::new(2, 3));
        let a3 = tree.add_window_under(layout, root, WindowId::new(1, 3));
        tree.select(b2);

        use Direction::*;
        assert_eq!(tree.traverse(a1, Left), None);
        assert_eq!(tree.traverse(a1, Up), None);
        assert_eq!(tree.traverse(a1, Down), None);
        assert_eq!(tree.traverse(a1, Right), Some(b2));
        assert_eq!(tree.traverse(a2, Left), Some(a1));
        assert_eq!(tree.traverse(a2, Up), None);
        assert_eq!(tree.traverse(a2, Down), None);
        assert_eq!(tree.traverse(a2, Right), Some(a3));
        assert_eq!(tree.traverse(b1, Left), Some(a1));
        assert_eq!(tree.traverse(b1, Up), None);
        assert_eq!(tree.traverse(b1, Down), Some(b2));
        assert_eq!(tree.traverse(b1, Right), Some(a3));
        assert_eq!(tree.traverse(b2, Left), Some(a1));
        assert_eq!(tree.traverse(b2, Up), Some(b1));
        assert_eq!(tree.traverse(b2, Down), Some(b3));
        assert_eq!(tree.traverse(b2, Right), Some(a3));
        assert_eq!(tree.traverse(b3, Left), Some(a1));
        assert_eq!(tree.traverse(b3, Up), Some(b2));
        assert_eq!(tree.traverse(b3, Down), None);
        assert_eq!(tree.traverse(b3, Right), Some(a3));
        assert_eq!(tree.traverse(a3, Left), Some(b2));
        assert_eq!(tree.traverse(a3, Up), None);
        assert_eq!(tree.traverse(a3, Down), None);
        assert_eq!(tree.traverse(a3, Right), None);
    }

    #[test]
    fn traverse_nested_same_orientation() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let a2 = tree.add_container(root, ContainerKind::Horizontal);
        let b1 = tree.add_window_under(layout, a2, WindowId::new(2, 1));
        let b2 = tree.add_window_under(layout, a2, WindowId::new(2, 2));
        let b3 = tree.add_window_under(layout, a2, WindowId::new(2, 3));
        let a3 = tree.add_window_under(layout, root, WindowId::new(1, 3));
        tree.select(b2);

        use Direction::*;
        assert_eq!(tree.traverse(a1, Left), None);
        assert_eq!(tree.traverse(a2, Left), Some(a1));
        assert_eq!(tree.traverse(b1, Left), Some(a1));
        assert_eq!(tree.traverse(b2, Left), Some(b1));
        assert_eq!(tree.traverse(b2, Left), Some(b1));
        assert_eq!(tree.traverse(b3, Left), Some(b2));
        assert_eq!(tree.traverse(a3, Left), Some(b3));
        assert_eq!(tree.traverse(a1, Right), Some(b1));
        assert_eq!(tree.traverse(a2, Right), Some(a3));
        assert_eq!(tree.traverse(b1, Right), Some(b2));
        assert_eq!(tree.traverse(b2, Right), Some(b3));
        assert_eq!(tree.traverse(b3, Right), Some(a3));
        assert_eq!(tree.traverse(a3, Right), None);
    }

    impl LayoutTree {
        #[track_caller]
        fn assert_children_are<const N: usize>(&self, children: [NodeId; N], parent: NodeId) {
            let actual: Vec<_> = parent.children(&self.tree.map).collect();
            assert_eq!(&children, actual.as_slice());
        }
    }

    #[test]
    fn move_node() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let a2 = tree.add_container(root, ContainerKind::Vertical);
        let b1 = tree.add_window_under(layout, a2, WindowId::new(2, 1));
        let b2 = tree.add_window_under(layout, a2, WindowId::new(2, 2));
        let b3 = tree.add_window_under(layout, a2, WindowId::new(2, 3));
        let a3 = tree.add_window_under(layout, root, WindowId::new(1, 3));
        tree.select(b2);
        tree.assert_children_are([a1, a2, a3], root);
        assert_eq!(b2, tree.selection(layout));

        tree.move_node(layout, b2, Direction::Left);
        tree.assert_children_are([a1, b2, a2, a3], root);
        assert_eq!(b2, tree.selection(layout));

        tree.move_node(layout, b2, Direction::Left);
        tree.assert_children_are([b2, a1, a2, a3], root);
        assert_eq!(b2, tree.selection(layout));

        tree.move_node(layout, a2, Direction::Left);
        tree.assert_children_are([b2, a2, a1, a3], root);
        assert_eq!(b2, tree.selection(layout));

        tree.select(a3);
        tree.move_node(layout, a3, Direction::Left);
        tree.assert_children_are([b2, a2, a3, a1], root);
        assert_eq!(a3, tree.selection(layout));

        tree.move_node(layout, a3, Direction::Left);
        tree.assert_children_are([b2, a2, a1], root);
        tree.assert_children_are([b1, b3, a3], a2);
        assert_eq!(a3, tree.selection(layout));

        tree.move_node(layout, a3, Direction::Right);
        tree.assert_children_are([b2, a2, a3, a1], root);
        tree.assert_children_are([b1, b3], a2);
        assert_eq!(a3, tree.selection(layout));

        tree.move_node(layout, b1, Direction::Down);
        tree.assert_children_are([b3, b1], a2);
        assert_eq!(a3, tree.selection(layout));

        tree.move_node(layout, b1, Direction::Up);
        tree.assert_children_are([b1, b3], a2);
        assert_eq!(a3, tree.selection(layout));

        tree.move_node(layout, b1, Direction::Up);
        let (old_root, root) = (root, tree.root(layout));
        tree.assert_children_are([b1, old_root], root);
        tree.assert_children_are([b2, b3, a3, a1], old_root);
        assert_eq!(ContainerKind::Vertical, tree.container_kind(root));
        assert_eq!(a3, tree.selection(layout));
        assert_eq!(Some(b1), tree.window_node(layout, WindowId::new(2, 1)));

        assert!(!tree.move_node(layout, root, Direction::Right));
    }

    #[test]
    fn move_node_removes_unnecessary_containers() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let a2 = tree.add_container(root, ContainerKind::Horizontal);
        let b1 = tree.add_window_under(layout, a2, WindowId::new(2, 1));
        let b2 = tree.add_window_under(layout, a2, WindowId::new(2, 2));
        let a3 = tree.add_window_under(layout, root, WindowId::new(1, 3));
        tree.assert_children_are([a1, a2, a3], root);

        // Ths resize should not affect the final size, because when the b nodes
        // are reparented they lose their original size.
        tree.resize(b2, 0.10, Direction::Left);

        tree.move_node(layout, b2, Direction::Right);
        tree.assert_children_are([a1, b1, b2, a3], root);
        let screen = rect(0, 0, 1000, 1000);
        assert_frames_are(
            tree.calculate_layout(layout, screen, &Config::default()),
            vec![
                (WindowId::new(1, 1), rect(0, 0, 250, 1000)),
                (WindowId::new(2, 1), rect(250, 0, 250, 1000)),
                (WindowId::new(2, 2), rect(500, 0, 250, 1000)),
                (WindowId::new(1, 3), rect(750, 0, 250, 1000)),
            ],
        );
    }

    #[test]
    fn move_node_removes_empty_containers() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let a2 = tree.add_container(root, ContainerKind::Vertical);
        let b1 = tree.add_window_under(layout, a2, WindowId::new(2, 1));
        let a3 = tree.add_window_under(layout, root, WindowId::new(1, 3));
        tree.assert_children_are([a1, a2, a3], root);

        tree.move_node(layout, b1, Direction::Right);
        tree.assert_children_are([a1, b1, a3], root);
    }

    #[test]
    fn remove_window_removes_unnecessary_containers() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let a2 = tree.add_container(root, ContainerKind::Vertical);
        let _b1 = tree.add_window_under(layout, a2, WindowId::new(2, 1));
        let b2 = tree.add_window_under(layout, a2, WindowId::new(2, 2));
        let a3 = tree.add_window_under(layout, root, WindowId::new(1, 3));
        tree.assert_children_are([a1, a2, a3], root);

        tree.remove_window(WindowId::new(2, 1));
        tree.assert_children_are([a1, b2, a3], root);
    }

    #[test]
    fn remove_window_removes_empty_containers() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let a2 = tree.add_container(root, ContainerKind::Vertical);
        let _b1 = tree.add_window_under(layout, a2, WindowId::new(2, 1));
        let a3 = tree.add_window_under(layout, root, WindowId::new(1, 3));
        tree.assert_children_are([a1, a2, a3], root);

        tree.remove_window(WindowId::new(2, 1));
        tree.assert_children_are([a1, a3], root);
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> CGRect {
        CGRect::new(
            CGPoint::new(f64::from(x), f64::from(y)),
            CGSize::new(f64::from(w), f64::from(h)),
        )
    }

    #[track_caller]
    fn assert_frames_are(
        left: impl IntoIterator<Item = (WindowId, CGRect)>,
        right: impl IntoIterator<Item = (WindowId, CGRect)>,
    ) {
        // Use BTreeMap for dedup and sorting.
        let left: BTreeMap<_, _> = left.into_iter().collect();
        let right: BTreeMap<_, _> = right.into_iter().collect();
        assert_eq!(left, right);
    }

    #[test]
    fn nest_in_container() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));

        // Calling on only child updates the (root) parent.
        assert_eq!(root, tree.nest_in_container(layout, a1, ContainerKind::Vertical));
        assert_eq!(ContainerKind::Vertical, tree.tree.data.size.kind(root));

        let a2 = tree.add_window_under(layout, root, WindowId::new(1, 2));
        tree.resize(a2, 0.10, Direction::Up);
        let orig_frames = tree.calculate_layout(layout, rect(0, 0, 1000, 1000), &Config::default());

        // Calling on child with siblings creates a new parent.
        // To keep the naming scheme consistent, rename the node a1 to b1
        // once it's nested a level deeper.
        tree.select(a1);
        let (b1, a1) = (a1, tree.nest_in_container(layout, a1, ContainerKind::Horizontal));
        tree.assert_children_are([a1, a2], root);
        tree.assert_children_are([b1], a1);
        assert_eq!(b1, tree.selection(layout));

        tree.select(a2);
        let (b2, a2) = (a2, tree.nest_in_container(layout, a2, ContainerKind::Horizontal));
        assert_eq!(b2, tree.selection(layout));
        tree.assert_children_are([a1, a2], root);
        tree.assert_children_are([b2], a2);
        assert_frames_are(
            orig_frames,
            tree.calculate_layout(layout, rect(0, 0, 1000, 1000), &Config::default()),
        );
        assert_eq!(b2, tree.selection(layout));

        // Calling on only child updates the (non-root) parent.
        assert_eq!(a2, tree.nest_in_container(layout, b2, ContainerKind::Horizontal));
        tree.assert_children_are([a1, a2], root);
        tree.assert_children_are([b2], a2);
        assert_eq!(b2, tree.selection(layout));

        // Calling on root works too.
        let (old_root, root) = (
            root,
            tree.nest_in_container(layout, root, ContainerKind::Vertical),
        );
        tree.assert_children_are([old_root], root);
        tree.assert_children_are([a1, a2], old_root);
        assert_eq!(b2, tree.selection(layout));

        let a3 = tree.add_window_under(layout, old_root, WindowId::new(1, 3));
        tree.assert_children_are([a1, a2, a3], old_root);
        assert_eq!(b2, tree.selection(layout));
    }

    #[test]
    fn resize() {
        // ┌─────┬─────┬─────┐
        // │     │ b1  │     │
        // │     +─────+     │
        // │ a1  │c1│c2│  a3 │
        // │     +─────+     │
        // │     │ b3  │     │
        // └─────┴─────┴─────┘
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let a2 = tree.add_container(root, ContainerKind::Vertical);
        let _b1 = tree.add_window_under(layout, a2, WindowId::new(2, 1));
        let b2 = tree.add_container(a2, ContainerKind::Horizontal);
        let _c1 = tree.add_window_under(layout, b2, WindowId::new(3, 1));
        let c2 = tree.add_window_under(layout, b2, WindowId::new(3, 2));
        let _b3 = tree.add_window_under(layout, a2, WindowId::new(2, 3));
        let _a3 = tree.add_window_under(layout, root, WindowId::new(1, 3));
        let screen = rect(0, 0, 3000, 3000);
        let config = &Config::default();

        let orig = vec![
            (WindowId::new(1, 1), rect(0, 0, 1000, 3000)),
            (WindowId::new(2, 1), rect(1000, 0, 1000, 1000)),
            (WindowId::new(3, 1), rect(1000, 1000, 500, 1000)),
            (WindowId::new(3, 2), rect(1500, 1000, 500, 1000)),
            (WindowId::new(2, 3), rect(1000, 2000, 1000, 1000)),
            (WindowId::new(1, 3), rect(2000, 0, 1000, 3000)),
        ];
        assert_frames_are(tree.calculate_layout(layout, screen, config), orig.clone());

        // We may want to have a mode that adjusts sizes so that only the
        // requested edge is resized. Notice that the width is redistributed
        // between c1 and c2 here.
        tree.resize(c2, 0.01, Direction::Right);
        assert_frames_are(
            tree.calculate_layout(layout, screen, config),
            [
                (WindowId::new(1, 1), rect(0, 0, 1000, 3000)),
                (WindowId::new(2, 1), rect(1000, 0, 1030, 1000)),
                (WindowId::new(3, 1), rect(1000, 1000, 515, 1000)),
                (WindowId::new(3, 2), rect(1515, 1000, 515, 1000)),
                (WindowId::new(2, 3), rect(1000, 2000, 1030, 1000)),
                (WindowId::new(1, 3), rect(2030, 0, 970, 3000)),
            ],
        );

        tree.resize(c2, -0.01, Direction::Right);
        assert_frames_are(tree.calculate_layout(layout, screen, config), orig.clone());

        tree.resize(c2, 0.01, Direction::Left);
        assert_frames_are(
            tree.calculate_layout(layout, screen, config),
            [
                (WindowId::new(1, 1), rect(0, 0, 1000, 3000)),
                (WindowId::new(2, 1), rect(1000, 0, 1000, 1000)),
                (WindowId::new(3, 1), rect(1000, 1000, 470, 1000)),
                (WindowId::new(3, 2), rect(1470, 1000, 530, 1000)),
                (WindowId::new(2, 3), rect(1000, 2000, 1000, 1000)),
                (WindowId::new(1, 3), rect(2000, 0, 1000, 3000)),
            ],
        );

        tree.resize(c2, -0.01, Direction::Left);
        assert_frames_are(tree.calculate_layout(layout, screen, config), orig.clone());

        tree.resize(b2, 0.01, Direction::Right);
        assert_frames_are(
            tree.calculate_layout(layout, screen, config),
            [
                (WindowId::new(1, 1), rect(0, 0, 1000, 3000)),
                (WindowId::new(2, 1), rect(1000, 0, 1030, 1000)),
                (WindowId::new(3, 1), rect(1000, 1000, 515, 1000)),
                (WindowId::new(3, 2), rect(1515, 1000, 515, 1000)),
                (WindowId::new(2, 3), rect(1000, 2000, 1030, 1000)),
                (WindowId::new(1, 3), rect(2030, 0, 970, 3000)),
            ],
        );

        tree.resize(b2, -0.01, Direction::Right);
        assert_frames_are(tree.calculate_layout(layout, screen, config), orig.clone());

        tree.resize(a1, 0.01, Direction::Right);
        assert_frames_are(
            tree.calculate_layout(layout, screen, config),
            [
                (WindowId::new(1, 1), rect(0, 0, 1030, 3000)),
                (WindowId::new(2, 1), rect(1030, 0, 970, 1000)),
                (WindowId::new(3, 1), rect(1030, 1000, 485, 1000)),
                (WindowId::new(3, 2), rect(1515, 1000, 485, 1000)),
                (WindowId::new(2, 3), rect(1030, 2000, 970, 1000)),
                (WindowId::new(1, 3), rect(2000, 0, 1000, 3000)),
            ],
        );

        tree.resize(a1, -0.01, Direction::Right);
        assert_frames_are(tree.calculate_layout(layout, screen, config), orig.clone());

        tree.resize(a1, 0.01, Direction::Left);
        assert_frames_are(tree.calculate_layout(layout, screen, config), orig.clone());
        tree.resize(a1, -0.01, Direction::Left);
        assert_frames_are(tree.calculate_layout(layout, screen, config), orig.clone());
    }

    #[test]
    fn set_frame_from_resize() {
        // ┌─────┬─────┬─────┐
        // │     │ b1  │     │
        // │     +─────+     │
        // │ a1  │c1│c2│  a3 │
        // │     +─────+     │
        // │     │ b3  │     │
        // └─────┴─────┴─────┘
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let a2 = tree.add_container(root, ContainerKind::Vertical);
        let _b1 = tree.add_window_under(layout, a2, WindowId::new(2, 1));
        let b2 = tree.add_container(a2, ContainerKind::Horizontal);
        let c1 = tree.add_window_under(layout, b2, WindowId::new(3, 1));
        let _c2 = tree.add_window_under(layout, b2, WindowId::new(3, 2));
        let _b3 = tree.add_window_under(layout, a2, WindowId::new(2, 3));
        let _a3 = tree.add_window_under(layout, root, WindowId::new(1, 3));
        let screen = rect(0, 0, 3000, 3000);
        let config = &Config::default();
        println!("{}", tree.draw_tree(layout));

        let orig = vec![
            (WindowId::new(1, 1), rect(0, 0, 1000, 3000)),
            (WindowId::new(2, 1), rect(1000, 0, 1000, 1000)),
            (WindowId::new(3, 1), rect(1000, 1000, 500, 1000)),
            (WindowId::new(3, 2), rect(1500, 1000, 500, 1000)),
            (WindowId::new(2, 3), rect(1000, 2000, 1000, 1000)),
            (WindowId::new(1, 3), rect(2000, 0, 1000, 3000)),
        ];
        assert_frames_are(tree.calculate_layout(layout, screen, config), orig.clone());

        // Moves should be rejected and ignored.
        tree.set_frame_from_resize(
            c1,
            rect(1000, 1000, 500, 1000),
            rect(1000, 1100, 500, 1000),
            screen,
            config,
        );
        assert_frames_are(tree.calculate_layout(layout, screen, config), orig.clone());

        tree.set_frame_from_resize(
            a1,
            rect(0, 0, 1000, 3000),
            rect(0, 0, 1010, 3000),
            screen,
            config,
        );
        assert_frames_are(
            tree.calculate_layout(layout, screen, config),
            [
                (WindowId::new(1, 1), rect(0, 0, 1010, 3000)),
                (WindowId::new(2, 1), rect(1010, 0, 990, 1000)),
                (WindowId::new(3, 1), rect(1010, 1000, 495, 1000)),
                (WindowId::new(3, 2), rect(1505, 1000, 495, 1000)),
                (WindowId::new(2, 3), rect(1010, 2000, 990, 1000)),
                (WindowId::new(1, 3), rect(2000, 0, 1000, 3000)),
            ],
        );

        tree.set_frame_from_resize(
            a1,
            rect(0, 0, 1010, 3000),
            rect(0, 0, 1000, 3000),
            screen,
            config,
        );
        assert_frames_are(tree.calculate_layout(layout, screen, config), orig.clone());

        tree.set_frame_from_resize(
            c1,
            rect(1000, 1000, 500, 1000),
            rect(900, 900, 600, 1100),
            screen,
            config,
        );
        assert_frames_are(
            tree.calculate_layout(layout, screen, config),
            [
                (WindowId::new(1, 1), rect(0, 0, 900, 3000)),
                (WindowId::new(2, 1), rect(900, 0, 1100, 900)),
                // This may not be what we actually want; notice the width
                // increase is redistributed across c1 and c2. In any case it's
                // confusing to have something called set_frame that results in
                // a different frame than requested..
                (WindowId::new(3, 1), rect(900, 900, 550, 1100)),
                (WindowId::new(3, 2), rect(1450, 900, 550, 1100)),
                (WindowId::new(2, 3), rect(900, 2000, 1100, 1000)),
                (WindowId::new(1, 3), rect(2000, 0, 1000, 3000)),
            ],
        );
    }

    /// A drag moves the edge by as many pixels as the user dragged it, whatever
    /// the gap settings. Regression test for #227.
    #[test]
    fn set_frame_from_resize_tracks_the_user_with_gaps() {
        for gap in [0.0, 8.0, 40.0] {
            for &vertical in &[false, true] {
                let mut tree = LayoutTree::new();
                let layout = tree.create_layout();
                let root = tree.root(layout);
                if vertical {
                    tree.set_container_kind(root, ContainerKind::Vertical);
                }
                let _a = tree.add_window_under(layout, root, WindowId::new(1, 1));
                let _b = tree.add_window_under(layout, root, WindowId::new(1, 2));
                let _c = tree.add_window_under(layout, root, WindowId::new(1, 3));
                let screen = rect(0, 39, 1800, 1130);
                let mut config = Config::default();
                config.settings.outer_gap = gap;
                config.settings.inner_gap = gap;
                let node = tree.window_node(layout, WindowId::new(1, 2)).unwrap();

                let frame_of = |tree: &LayoutTree, wid| {
                    tree.calculate_layout(layout, screen, &config)
                        .into_iter()
                        .find(|&(w, _)| w == wid)
                        .unwrap()
                        .1
                };

                // Drag the middle window's leading edge in increments, as a
                // stream of AXWindowResized notifications would report it.
                let mut cur = frame_of(&tree, WindowId::new(1, 2));
                for _ in 0..5 {
                    let new = if vertical {
                        CGRect::new(
                            CGPoint::new(cur.origin.x, cur.origin.y + 17.),
                            CGSize::new(cur.size.width, cur.size.height - 17.),
                        )
                    } else {
                        CGRect::new(
                            CGPoint::new(cur.origin.x + 34., cur.origin.y),
                            CGSize::new(cur.size.width - 34., cur.size.height),
                        )
                    };
                    tree.set_frame_from_resize(node, cur, new, screen, &config);
                    assert_eq!(
                        frame_of(&tree, WindowId::new(1, 2)),
                        new,
                        "gap={gap} vertical={vertical}: layout disagrees with the user's frame"
                    );
                    cur = new;
                }
            }
        }
    }

    #[test]
    fn set_frame_from_resize_tracks_the_user_with_size_locks() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let _a = tree.add_window_under(layout, root, w(1, 1));
        let _b = tree.add_window_under(layout, root, w(1, 2));
        let _c = tree.add_window_under(layout, root, w(1, 3));
        let screen = rect(0, 0, 1200, 1200);
        let config = Config::default();
        let node = tree.window_node(layout, w(1, 2)).unwrap();

        // A is locked at half the screen while B's trailing edge is dragged
        // into C.
        tree.set_size_lock(w(1, 1), 0.5);

        let frame_of = |tree: &LayoutTree, wid| {
            tree.calculate_layout(layout, screen, &config)
                .into_iter()
                .find(|&(w, _)| w == wid)
                .unwrap()
                .1
        };

        let mut cur = frame_of(&tree, w(1, 2));
        for _ in 0..5 {
            let new = CGRect::new(
                CGPoint::new(cur.origin.x, cur.origin.y),
                CGSize::new(cur.size.width + 20., cur.size.height),
            );
            tree.set_frame_from_resize(node, cur, new, screen, &config);
            assert_eq!(
                frame_of(&tree, w(1, 2)),
                new,
                "layout disagrees with the user's frame while another window is locked"
            );
            cur = new;
        }

        // B's leading edge is frozen, so dragging it does nothing.
        let frozen = frame_of(&tree, w(1, 2));
        let dragged = CGRect::new(
            CGPoint::new(frozen.origin.x + 40., frozen.origin.y),
            CGSize::new(frozen.size.width - 40., frozen.size.height),
        );
        tree.set_frame_from_resize(node, frozen, dragged, screen, &config);
        assert_eq!(
            frame_of(&tree, w(1, 2)),
            frozen,
            "a size locked boundary should not move"
        );
    }

    #[test]
    fn visible_windows_under_simple() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let _a1 = tree.add_window_under(layout, root, w(1, 1));
        let _a2 = tree.add_window_under(layout, root, w(1, 2));
        let _a3 = tree.add_window_under(layout, root, w(1, 3));

        let mut windows = tree.visible_windows_under(root);
        windows.sort();
        assert_eq!(windows, vec![w(1, 1), w(1, 2), w(1, 3)]);

        let windows = tree.visible_windows_under(_a1);
        assert_eq!(windows, vec![w(1, 1)]);
    }

    #[test]
    fn visible_windows_under_with_groups() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);

        let group = tree.add_container(root, ContainerKind::Stacked);
        let _tab1 = tree.add_window_under(layout, group, w(1, 1));
        let tab2 = tree.add_window_under(layout, group, w(1, 2));
        let _tab3 = tree.add_window_under(layout, group, w(1, 3));

        tree.select(tab2);

        // visible_windows_under should only return the selected tab from the group
        let windows = tree.visible_windows_under(group);
        assert_eq!(windows, vec![w(1, 2)]);

        // Add another non-group window
        let _a1 = tree.add_window_under(layout, root, w(2, 1));

        let mut windows = tree.visible_windows_under(root);
        windows.sort();
        assert_eq!(windows, vec![w(1, 2), w(2, 1)]);
    }

    #[test]
    fn visible_windows_under_nested_groups() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);

        // Create nested group structure
        let outer_group = tree.add_container(root, ContainerKind::Stacked);
        let inner_group = tree.add_container(outer_group, ContainerKind::Tabbed);
        let tab1 = tree.add_window_under(layout, inner_group, w(1, 1));
        let _tab2 = tree.add_window_under(layout, inner_group, w(1, 2));
        let _outer_tab = tree.add_window_under(layout, outer_group, w(2, 1));

        // Select tab1 in inner group - this should set up the selection path so
        // that outer_group has inner_group selected, and inner_group has tab1
        // selected.
        tree.select(tab1);

        let windows = tree.visible_windows_under(outer_group);
        assert_eq!(windows, vec![w(1, 1)]);
    }

    #[test]
    fn select_returning_surfaced_windows_no_groups() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, w(1, 1));
        let a2 = tree.add_window_under(layout, root, w(1, 2));

        // Selecting in a non-group structure should return only the selected window.
        let windows = tree.select_returning_surfaced_windows(a1);
        assert_eq!(windows, vec![w(1, 1)]);

        let windows = tree.select_returning_surfaced_windows(a2);
        assert_eq!(windows, vec![w(1, 2)]);
    }

    #[test]
    fn select_returning_surfaced_windows_with_groups() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);

        let group = tree.add_container(root, ContainerKind::Stacked);
        let tab1 = tree.add_window_under(layout, group, w(1, 1));
        let tab2 = tree.add_window_under(layout, group, w(1, 2));
        let _tab3 = tree.add_window_under(layout, group, w(1, 3));
        tree.select(tab1);

        // Selecting tab2 should return all visible windows under the group
        let windows = tree.select_returning_surfaced_windows(tab2);
        assert_eq!(windows, vec![w(1, 2)]);
        assert_eq!(tree.selection(layout), tab2);
    }

    #[test]
    fn select_returning_surfaced_windows_nested_groups() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);

        // Create nested structure with groups
        let container = tree.add_container(root, ContainerKind::Horizontal);
        let group1 = tree.add_container(container, ContainerKind::Tabbed);
        let tab1 = tree.add_window_under(layout, group1, w(1, 1));
        let _tab2 = tree.add_window_under(layout, group1, w(1, 2));

        let group2 = tree.add_container(container, ContainerKind::Stacked);
        let tab3 = tree.add_container(group2, ContainerKind::Horizontal);
        let tab3_1 = tree.add_window_under(layout, tab3, w(2, 1));
        let _tab3_2 = tree.add_window_under(layout, tab3, w(2, 2));
        let _tab4 = tree.add_window_under(layout, group2, w(2, 3));

        tree.select(tab1);

        // Now select tab3 in group2 - this should reveal the group containing tab3
        let mut windows = tree.select_returning_surfaced_windows(tab3_1);
        windows.sort();
        assert_eq!(windows, vec![w(2, 1), w(2, 2)]);
        assert_eq!(tree.selection(layout), tab3_1);
    }

    #[test]
    fn select_returning_surfaced_windows_highest_group_revealed() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);

        // Create deeply nested groups
        let outer_group = tree.add_container(root, ContainerKind::Stacked);
        let middle_container = tree.add_container(outer_group, ContainerKind::Horizontal);
        let inner_group = tree.add_container(middle_container, ContainerKind::Stacked);
        let tab1 = tree.add_window_under(layout, inner_group, w(1, 1));
        let tab2 = tree.add_window_under(layout, inner_group, w(1, 2));
        let _other_window = tree.add_window_under(layout, middle_container, w(2, 1));
        let outer_tab = tree.add_window_under(layout, outer_group, w(3, 1));

        tree.select(tab1);

        // Should surface windows from the inner group since that's the highest
        // group that changed.
        let windows = tree.select_returning_surfaced_windows(tab2);
        assert_eq!(windows, vec![w(1, 2)]);

        tree.select(outer_tab);

        // Should surface any visible windows in middle_container.
        let mut windows = tree.select_returning_surfaced_windows(tab2);
        windows.sort();
        assert_eq!(windows, vec![w(1, 2), w(2, 1)]);
    }

    #[test]
    fn traverse_scroll_wrapping_wraps_right() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_scroll_layout();
        let w1 = tree.add_window_to_scroll_column(layout, w(1, 1), true);
        tree.select(w1);
        let w2 = tree.add_window_to_scroll_column(layout, w(1, 2), true);
        tree.select(w2);
        let w3 = tree.add_window_to_scroll_column(layout, w(1, 3), true);
        tree.select(w3);

        let result = tree.traverse_scroll_wrapping(layout, w3, Direction::Right);
        assert_eq!(result, Some(w1));
    }

    #[test]
    fn traverse_scroll_wrapping_wraps_left() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_scroll_layout();
        let w1 = tree.add_window_to_scroll_column(layout, w(1, 1), true);
        tree.select(w1);
        let w2 = tree.add_window_to_scroll_column(layout, w(1, 2), true);
        tree.select(w2);
        let _w3 = tree.add_window_to_scroll_column(layout, w(1, 3), true);

        let result = tree.traverse_scroll_wrapping(layout, w1, Direction::Left);
        assert!(result.is_some());
    }

    #[test]
    fn traverse_scroll_wrapping_single_column_returns_none() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_scroll_layout();
        let w1 = tree.add_window_to_scroll_column(layout, w(1, 1), true);

        let result = tree.traverse_scroll_wrapping(layout, w1, Direction::Right);
        assert_eq!(result, None);
    }

    #[test]
    fn traverse_scroll_wrapping_empty_returns_none() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_scroll_layout();
        let root = tree.root(layout);

        let result = tree.traverse_scroll_wrapping(layout, root, Direction::Right);
        assert_eq!(result, None);
    }

    #[test]
    fn is_visible_without_groups() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let a1 = tree.add_window_under(layout, root, w(1, 1));
        let a2 = tree.add_window_under(layout, root, w(1, 2));

        assert!(tree.is_visible(root));
        assert!(tree.is_visible(a1));
        assert!(tree.is_visible(a2));
    }

    #[test]
    fn is_visible_with_group() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let group = tree.add_container(root, ContainerKind::Stacked);
        let tab1 = tree.add_window_under(layout, group, w(1, 1));
        let tab2 = tree.add_window_under(layout, group, w(1, 2));
        let tab3 = tree.add_window_under(layout, group, w(1, 3));

        tree.select(tab2);

        assert!(tree.is_visible(root));
        assert!(tree.is_visible(group));
        assert!(!tree.is_visible(tab1));
        assert!(tree.is_visible(tab2));
        assert!(!tree.is_visible(tab3));
    }

    #[test]
    fn is_visible_nested_groups() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let outer_group = tree.add_container(root, ContainerKind::Stacked);
        let inner_group = tree.add_container(outer_group, ContainerKind::Tabbed);
        let tab1 = tree.add_window_under(layout, inner_group, w(1, 1));
        let tab2 = tree.add_window_under(layout, inner_group, w(1, 2));
        let outer_tab = tree.add_window_under(layout, outer_group, w(2, 1));

        // Select tab1: inner_group selects tab1, outer_group selects inner_group.
        tree.select(tab1);
        assert!(tree.is_visible(tab1));
        assert!(!tree.is_visible(tab2));
        assert!(!tree.is_visible(outer_tab));

        // Select outer_tab: outer_group no longer selects inner_group.
        tree.select(outer_tab);
        assert!(!tree.is_visible(tab1));
        assert!(!tree.is_visible(tab2));
        assert!(tree.is_visible(outer_tab));
    }

    #[test]
    fn size_lock_freezes_a_share_of_the_screen() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        tree.add_window_under(layout, root, w(1, 1));
        tree.add_window_under(layout, root, w(1, 2));
        tree.add_window_under(layout, root, w(1, 3));

        let screen = rect(0, 0, 1000, 1000);
        tree.set_size_lock(w(1, 1), 0.5);
        assert_frames_are(
            [
                (w(1, 1), rect(0, 0, 500, 1000)),
                (w(1, 2), rect(500, 0, 250, 1000)),
                (w(1, 3), rect(750, 0, 250, 1000)),
            ],
            tree.calculate_layout(layout, screen, &Config::default()),
        );
    }

    #[test]
    fn size_lock_freezes_a_nested_window() {
        // [A | [B / C]], with B locked at half the screen.
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        tree.add_window_under(layout, root, w(1, 1));
        let column = tree.add_container(root, ContainerKind::Vertical);
        tree.add_window_under(layout, column, w(1, 2));
        tree.add_window_under(layout, column, w(1, 3));

        tree.set_size_lock(w(1, 2), 0.5);
        assert_frames_are(
            [
                (w(1, 1), rect(0, 0, 333, 1000)),
                (w(1, 2), rect(333, 0, 667, 750)),
                (w(1, 3), rect(333, 750, 667, 250)),
            ],
            tree.calculate_layout(layout, rect(0, 0, 1000, 1000), &Config::default()),
        );
    }

    #[test]
    fn size_locked_share_survives_new_windows() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        tree.add_window_under(layout, root, w(1, 1));
        tree.add_window_under(layout, root, w(1, 2));
        tree.set_size_lock(w(1, 1), 0.5);

        tree.add_window_under(layout, root, w(1, 3));
        assert_frames_are(
            [
                (w(1, 1), rect(0, 0, 500, 1000)),
                (w(1, 2), rect(500, 0, 250, 1000)),
                (w(1, 3), rect(750, 0, 250, 1000)),
            ],
            tree.calculate_layout(layout, rect(0, 0, 1000, 1000), &Config::default()),
        );
    }

    #[test]
    fn size_locks_squeeze_to_make_room_for_new_windows() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        tree.add_window_under(layout, root, w(1, 1));
        tree.add_window_under(layout, root, w(1, 2));
        tree.set_size_lock(w(1, 1), 0.5);
        tree.set_size_lock(w(1, 2), 0.5);

        // The locks cover the whole screen, so they scale down together to
        // leave the new window a usable share.
        tree.add_window_under(layout, root, w(1, 3));
        assert_frames_are(
            [
                (w(1, 1), rect(0, 0, 450, 1000)),
                (w(1, 2), rect(450, 0, 450, 1000)),
                (w(1, 3), rect(900, 0, 100, 1000)),
            ],
            tree.calculate_layout(layout, rect(0, 0, 1000, 1000), &Config::default()),
        );
    }

    #[test]
    fn size_lock_on_a_group_applies_to_the_group_rect() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        tree.add_window_under(layout, root, w(1, 1));
        let group = tree.add_container(root, ContainerKind::Tabbed);
        tree.add_window_under(layout, group, w(1, 2));
        tree.add_window_under(layout, group, w(1, 3));

        tree.set_size_lock(w(1, 2), 0.5);
        assert_frames_are(
            [
                (w(1, 1), rect(0, 0, 500, 1000)),
                (w(1, 2), rect(500, 6, 500, 994)),
                (w(1, 3), rect(500, 6, 500, 994)),
            ],
            tree.calculate_layout(layout, rect(0, 0, 1000, 1000), &Config::default()),
        );
    }

    #[test]
    fn a_lone_locked_window_leaves_the_rest_of_the_screen_empty() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        tree.add_window_under(layout, root, w(1, 1));

        tree.set_size_lock(w(1, 1), 0.5);
        assert_frames_are(
            [(w(1, 1), rect(0, 0, 500, 1000))],
            tree.calculate_layout(layout, rect(0, 0, 1000, 1000), &Config::default()),
        );
    }

    #[test]
    fn clearing_a_size_lock_restores_the_weights() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        tree.add_window_under(layout, root, w(1, 1));
        tree.add_window_under(layout, root, w(1, 2));
        let original = tree.calculate_layout(layout, rect(0, 0, 1000, 1000), &Config::default());

        tree.set_size_lock(w(1, 1), 0.5);
        assert!(tree.clear_size_lock(w(1, 1)));
        assert!(!tree.clear_size_lock(w(1, 1)));
        assert_frames_are(
            original,
            tree.calculate_layout(layout, rect(0, 0, 1000, 1000), &Config::default()),
        );
    }

    #[test]
    fn size_locks_do_not_apply_to_scroll_layouts() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_scroll_layout();
        let _root = tree.root(layout);
        tree.add_window_to_scroll_column(layout, w(1, 1), true);
        tree.add_window_to_scroll_column(layout, w(1, 2), true);
        let original = tree.calculate_layout(layout, rect(0, 0, 1000, 1000), &Config::default());

        tree.set_size_lock(w(1, 1), 0.5);
        assert_frames_are(
            original,
            tree.calculate_layout(layout, rect(0, 0, 1000, 1000), &Config::default()),
        );
    }
}

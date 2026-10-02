// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::{Reactor, WindowId};
use crate::model::contexts::{ContextId, ContextKey};
use crate::model::native_tabs::TabGroup;

pub(super) struct Group {
    pub snapshot: TabGroup,
    pub owner: WindowId,
    representative: WindowId,
    layout_members: Vec<WindowId>,
    policy: Option<(Vec<ContextId>, bool)>,
}

impl Reactor {
    /// A closed tab is absent, not an unidentified button. Keep only the
    /// representative's leaf until a surviving tab takes its place.
    pub(super) fn native_window_destroyed(&mut self, window: WindowId) -> bool {
        let mut keep_leaf = false;
        let mut remove = Vec::new();
        for group in self.native_tabs.values_mut() {
            group.snapshot.members.retain(|member| *member != Some(window));
            if group.snapshot.members.is_empty() {
                if group.representative != window {
                    remove.push(group.representative);
                }
            } else if group.representative == window {
                keep_leaf = true;
            }
            if group.owner == window
                && let Some(owner) = group.snapshot.members.iter().flatten().next()
            {
                group.owner = *owner;
            }
        }
        self.native_tabs.retain(|_, group| !group.snapshot.members.is_empty());
        for window in remove {
            self.send_layout_event(crate::actor::layout::LayoutEvent::WindowRemoved(window));
        }
        keep_leaf
    }

    pub(super) fn native_group(&self, window: WindowId) -> Option<&Group> {
        self.native_tabs
            .values()
            .find(|group| group.snapshot.members.contains(&Some(window)))
    }

    pub(super) fn native_tabs_changed(&mut self, window: WindowId, snapshot: Option<TabGroup>) {
        self.unavailable_tabs.remove(&window);
        if let Some(group) = &snapshot {
            let valid = group.selected == window
                && !group.members.is_empty()
                && group.members.len() <= 256
                && group.members.contains(&Some(window))
                && group.members.iter().flatten().all(|wid| wid.pid == window.pid)
                && group.members.iter().flatten().enumerate().all(|(i, wid)| {
                    !group.members.iter().flatten().take(i).any(|other| other == wid)
                });
            if !valid {
                self.unavailable_tabs.insert(window);
                self.fall_back_for_unidentified_tabs();
                return;
            }
            for member in group.members.iter().flatten() {
                self.unavailable_tabs.remove(member);
            }
        }
        let key = snapshot.as_ref().map(|group| (window.pid, group.id));
        let moved: Vec<_> = snapshot.as_ref().map_or_else(
            || vec![window],
            |group| group.members.iter().flatten().copied().collect(),
        );
        // Only positive identity evidence can remove an old association.
        for (other, group) in &mut self.native_tabs {
            if Some(*other) != key {
                group
                    .snapshot
                    .members
                    .retain(|member| member.is_none_or(|wid| !moved.contains(&wid)));
                if moved.contains(&group.owner)
                    && let Some(owner) = group.snapshot.members.iter().flatten().next()
                {
                    group.owner = *owner;
                }
                if moved.contains(&group.representative) {
                    group.representative = group.owner;
                }
            }
        }
        if let Some(snapshot) = snapshot {
            let key = (window.pid, snapshot.id);
            match self.native_tabs.get_mut(&key) {
                Some(group) => group.snapshot = snapshot,
                None => {
                    self.native_tabs.insert(
                        key,
                        Group {
                            snapshot,
                            owner: window,
                            representative: window,
                            layout_members: vec![],
                            policy: None,
                        },
                    );
                }
            }
        }
        self.native_tabs.retain(|_, group| !group.snapshot.members.is_empty());
        self.reconcile_native_tab_membership(window.pid);
        self.fall_back_for_unidentified_tabs();
    }

    pub(super) fn remember_native_tab_policies(&mut self) {
        if !self.contexts_enabled() {
            return;
        }
        for group in self.native_tabs.values_mut() {
            if self.windows.contains_key(&group.owner)
                && group.snapshot.members.contains(&Some(group.owner))
            {
                group.policy = Some((
                    self.contexts.contexts_of(group.owner),
                    self.contexts.is_pinned(group.owner),
                ));
            }
        }
    }

    pub(super) fn reconcile_native_tab_membership(&mut self, pid: crate::actor::app::pid_t) {
        let replacements: Vec<_> = self
            .native_tabs
            .iter()
            .filter(|((app, _), group)| {
                *app == pid
                    && self.windows.contains_key(&group.snapshot.selected)
                    && group.snapshot.members.contains(&Some(group.snapshot.selected))
            })
            .map(|(key, group)| {
                (
                    *key,
                    group.representative,
                    group.snapshot.selected,
                    group.snapshot.members.iter().flatten().copied().collect::<Vec<_>>(),
                )
            })
            .filter(|(key, previous, selected, members)| {
                previous != selected || self.native_tabs[key].layout_members != *members
            })
            .collect();
        for (key, previous, selected, members) in replacements {
            self.send_layout_event(crate::actor::layout::LayoutEvent::NativeTabSelected {
                previous,
                selected,
                members: members.clone(),
            });
            self.native_tabs.get_mut(&key).unwrap().representative = selected;
            self.native_tabs.get_mut(&key).unwrap().layout_members = members.clone();
            self.restore_selected_native_tab(previous, selected, &members);
            if previous != selected && !self.windows.contains_key(&previous) {
                self.send_layout_event(crate::actor::layout::LayoutEvent::WindowRemoved(previous));
            }
        }
        if !self.contexts_enabled() {
            return;
        }
        self.remember_native_tab_policies();
        let groups: Vec<_> = self
            .native_tabs
            .iter()
            .filter(|((app, _), _)| *app == pid)
            .filter(|(_, group)| {
                group
                    .snapshot
                    .members
                    .iter()
                    .flatten()
                    .all(|wid| self.windows.contains_key(wid))
            })
            .filter_map(|(key, group)| Some((*key, group.snapshot.clone(), group.policy.clone()?)))
            .collect();
        let mut changed = false;
        for (key, group, (contexts, pinned)) in groups {
            let added =
                group.members.iter().flatten().any(|wid| self.added_since_switch.contains(wid));
            for wid in group.members.iter().flatten().copied() {
                let Some(desc) = self.window_desc(wid) else { continue };
                for old in self.contexts.contexts_of(wid) {
                    if !contexts.contains(&old) {
                        changed |= self.contexts.remove_window(old, wid).unwrap_or(false);
                    }
                }
                for &id in &contexts {
                    changed |= self.contexts.add_window(id, &desc).unwrap_or(false);
                }
                changed |= if pinned {
                    self.contexts.pin(&desc)
                } else {
                    self.contexts.unpin(wid)
                };
                if added {
                    self.added_since_switch.insert(wid);
                } else {
                    self.added_since_switch.remove(&wid);
                }
            }
            self.native_tabs.get_mut(&key).unwrap().owner = group.selected;
        }
        if changed {
            self.save_contexts();
        }
    }

    pub(super) fn unidentified_native_tabs(&self) -> bool {
        let relevant = |wid: WindowId| {
            wid.pid != std::process::id() as i32
                && self.window_on_screen(wid)
                && self
                    .layout_window_info(wid)
                    .is_some_and(|info| !self.layout.is_untracked(&info))
        };
        self.unavailable_tabs.iter().copied().any(relevant)
            || self.native_tabs.values().any(|group| {
                group.snapshot.members.contains(&Some(group.snapshot.selected))
                    && relevant(group.snapshot.selected)
                    && (!group.snapshot.identified()
                        || group
                            .snapshot
                            .members
                            .iter()
                            .flatten()
                            .any(|wid| !self.windows.contains_key(wid)))
            })
    }

    pub(super) fn fall_back_for_unidentified_tabs(&mut self) {
        if self.contexts_enabled()
            && self.unidentified_native_tabs()
            && self
                .screens
                .iter()
                .enumerate()
                .any(|(i, _)| self.screen_key(i) != ContextKey::Everything)
        {
            self.abort_failed_parking(Self::unidentified_tabs_message().into());
        }
    }

    pub(super) fn unidentified_tabs_message() -> &'static str {
        "Native tabs are not fully identified. Select each tab once, then switch contexts. Showing Everything"
    }
}

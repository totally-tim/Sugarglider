// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Native tab identities observed by an app actor. No geometry or title matching.

use serde::{Deserialize, Serialize};

use crate::actor::app::WindowId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabGroup {
    pub id: u64,
    pub selected: WindowId,
    /// None means macOS has not exposed this tab's window identity yet.
    pub members: Vec<Option<WindowId>>,
}

impl TabGroup {
    pub fn identified(&self) -> bool {
        self.members.iter().all(Option::is_some)
    }
}

struct Group<E> {
    element: E,
    tabs: Vec<(E, Option<WindowId>)>,
    snapshot: TabGroup,
}

/// E compares OS object identity, not its address, hash, title, or frame.
pub struct TabTracker<E> {
    groups: Vec<Group<E>>,
    next_id: u64,
}

impl<E> Default for TabTracker<E> {
    fn default() -> Self {
        Self { groups: Vec::new(), next_id: 1 }
    }
}

impl<E: Eq + Clone> TabTracker<E> {
    pub fn bar_for_window(&self, window: WindowId) -> Option<&E> {
        self.groups
            .iter()
            .find(|group| group.tabs.iter().any(|(_, wid)| *wid == Some(window)))
            .map(|group| &group.element)
    }

    pub fn observe(
        &mut self,
        window: WindowId,
        element: E,
        tabs: Vec<E>,
        selected: usize,
    ) -> Option<TabGroup> {
        if tabs.is_empty()
            || tabs.len() > 256
            || selected >= tabs.len()
            || tabs.iter().enumerate().any(|(i, tab)| tabs[..i].contains(tab))
        {
            return None;
        }
        let existing = self.groups.iter().position(|group| group.element == element);
        if existing.is_none() && self.groups.len() >= 1024 {
            self.groups
                .retain(|group| group.tabs.iter().any(|(_, window)| window.is_some()));
            if self.groups.len() >= 1024 {
                return None;
            }
        }
        let mut bindings: Vec<_> = tabs
            .into_iter()
            .map(|tab| {
                let known = self
                    .groups
                    .iter()
                    .flat_map(|group| &group.tabs)
                    .find(|(other, _)| *other == tab)
                    .and_then(|(_, window)| *window);
                (tab, known)
            })
            .collect();
        // A live button cannot identify two windows, and two live buttons in
        // one bar cannot identify the same window. Keep the last valid map.
        if bindings[selected].1.is_some_and(|known| known != window)
            || bindings
                .iter()
                .enumerate()
                .any(|(i, (_, known))| i != selected && *known == Some(window))
        {
            return None;
        }
        // A window can move to another tab bar. Only its currently selected
        // button is positive evidence of the new binding.
        for group in &mut self.groups {
            for (_, bound) in &mut group.tabs {
                if *bound == Some(window) {
                    *bound = None;
                }
            }
        }
        bindings[selected].1 = Some(window);
        let id = existing.map(|i| self.groups[i].snapshot.id).unwrap_or_else(|| {
            let id = self.next_id;
            self.next_id += 1;
            id
        });
        let snapshot = TabGroup {
            id,
            selected: window,
            members: bindings.iter().map(|(_, window)| *window).collect(),
        };
        let group = Group {
            element,
            tabs: bindings,
            snapshot: snapshot.clone(),
        };
        if let Some(i) = existing {
            self.groups[i] = group;
        } else {
            self.groups.push(group);
        }
        Some(snapshot)
    }

    pub fn forget_window(&mut self, window: WindowId) {
        for group in &mut self.groups {
            for (_, bound) in &mut group.tabs {
                if *bound == Some(window) {
                    *bound = None;
                }
            }
        }
        // The last identified tab may close before an unvisited tab becomes
        // selected. Keep the bar identity so that its membership survives.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(index: u32) -> WindowId {
        WindowId::new(1, index)
    }

    #[test]
    fn closing_the_only_identified_tab_keeps_the_group_identity() {
        let mut tracker = TabTracker::default();
        let first = tracker.observe(w(1), 10, vec![20, 21], 0).unwrap();
        tracker.forget_window(w(1));
        let next = tracker.observe(w(2), 10, vec![21], 0).unwrap();
        assert_eq!(next.id, first.id);
        assert_eq!(next.members, [Some(w(2))]);
    }

    #[test]
    fn identifies_only_the_selected_tab_and_keeps_identity_across_selection() {
        let mut tracker = TabTracker::default();
        let first = tracker.observe(w(1), 10, vec![20, 21], 0).unwrap();
        assert_eq!(first.members, [Some(w(1)), None]);
        assert!(!first.identified());
        let second = tracker.observe(w(2), 10, vec![20, 21], 1).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.members, [Some(w(1)), Some(w(2))]);
        assert!(second.identified());
    }

    #[test]
    fn unrelated_group_cannot_inherit_an_unselected_window() {
        let mut tracker = TabTracker::default();
        let first = tracker.observe(w(1), 10, vec![20, 21], 0).unwrap();
        let other = tracker.observe(w(2), 11, vec![22, 23], 1).unwrap();
        assert_ne!(first.id, other.id);
        assert_eq!(other.members, [None, Some(w(2))]);
    }

    #[test]
    fn moving_and_reordering_a_tab_preserves_its_explicit_binding() {
        let mut tracker = TabTracker::default();
        tracker.observe(w(1), 10, vec![20, 21], 0).unwrap();
        tracker.observe(w(2), 10, vec![20, 21], 1).unwrap();
        let moved = tracker.observe(w(3), 11, vec![21, 22], 1).unwrap();
        assert_eq!(moved.members, [Some(w(2)), Some(w(3))]);
        let reordered = tracker.observe(w(2), 11, vec![22, 21], 1).unwrap();
        assert_eq!(reordered.members, [Some(w(3)), Some(w(2))]);
    }

    #[test]
    fn closing_and_replacing_a_tab_never_reuses_its_window_identity() {
        let mut tracker = TabTracker::default();
        tracker.observe(w(1), 10, vec![20, 21], 0).unwrap();
        tracker.observe(w(2), 10, vec![20, 21], 1).unwrap();
        tracker.forget_window(w(2));
        let replaced = tracker.observe(w(1), 10, vec![20, 22], 0).unwrap();
        assert_eq!(replaced.members, [Some(w(1)), None]);
    }

    #[test]
    fn rejects_ambiguous_or_unbounded_observations_without_mutation() {
        let mut tracker = TabTracker::default();
        assert!(tracker.observe(w(1), 10, vec![20, 20], 0).is_none());
        assert!(tracker.observe(w(1), 10, vec![20], 1).is_none());
        assert!(tracker.observe(w(1), 10, (0..257).collect(), 0).is_none());
        assert!(tracker.groups.is_empty());
    }

    #[test]
    fn conflicting_live_identities_preserve_the_last_valid_binding() {
        let mut tracker = TabTracker::default();
        let first = tracker.observe(w(1), 10, vec![20, 21], 0).unwrap();
        assert!(tracker.observe(w(2), 10, vec![20, 21], 0).is_none());
        assert!(tracker.observe(w(1), 10, vec![20, 21], 1).is_none());
        assert_eq!(tracker.observe(w(1), 10, vec![20, 21], 0), Some(first));
        assert_eq!(
            tracker.observe(w(2), 10, vec![21, 20], 0).unwrap().members,
            [Some(w(2)), Some(w(1))]
        );
    }
}

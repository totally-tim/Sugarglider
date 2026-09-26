// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Membership: which contexts a window belongs to. The reactor decides it when
//! it first sees a window, and keeps it current as windows change and close.
//! The design is in `docs/specs/contexts.md`.

use redact::Secret;
use tracing::{debug, info};

use super::contexts::Apply;
use super::{ContextRef, Reactor, RecordRef};
use crate::actor::app::{WindowId, pid_t};
use crate::actor::contexts_snapshot::app_name;
use crate::model::contexts::{
    Arrival, ContextId, ContextKey, MatchPass, RecordLink, Slot, WindowDesc, plan_switch,
};

impl Reactor {
    /// Decides the membership of windows that the reactor sees for the first
    /// time. The reactor's windows are the windows it has seen, so a window
    /// that comes back from being minimized, from a hidden app, or from another
    /// Space is not new.
    ///
    /// Windows found before `StartupComplete` were open before Sugarglider
    /// started. They rejoin the contexts whose records they match, or stay
    /// unsorted. Later windows rejoin the contexts whose records they match, or
    /// else join the context that their screen shows.
    ///
    /// A window waits when the window server hasn't listed it yet: its layer
    /// decides whether the layout tracks it, and only tracked windows join a
    /// context. The window server's list decides the waiting windows. This
    /// keeps a panel, whose layer is reported after its creation, out of the
    /// contexts.
    ///
    /// Sugarglider's own windows, windows the layout doesn't track, and windows
    /// of apps that haven't registered get no membership. With contexts off
    /// nothing is decided; the windows rejoin their contexts when contexts are
    /// turned on.
    pub(super) fn windows_first_seen(&mut self, wids: &[WindowId]) -> Vec<WindowId> {
        if !self.contexts_enabled() {
            return Vec::new();
        }
        let (ready, waiting): (Vec<WindowId>, Vec<WindowId>) =
            wids.iter().copied().partition(|&wid| self.window_layer_known(wid));
        self.pending_first_seen.extend(waiting);
        if ready.is_empty() {
            return ready;
        }
        self.decide_membership(&ready);
        ready
    }

    /// Decides the membership of the windows that waited for the window
    /// server to list them, now that its list has arrived. The caller applies
    /// the focus that waited for them.
    pub(super) fn decide_pending_membership(&mut self) -> Vec<WindowId> {
        let ready: Vec<WindowId> = self
            .pending_first_seen
            .iter()
            .copied()
            .filter(|&wid| self.window_layer_known(wid))
            .collect();
        if ready.is_empty() {
            return ready;
        }
        for wid in &ready {
            self.pending_first_seen.remove(wid);
        }
        self.decide_membership(&ready);
        ready
    }

    /// Whether the window server has listed the window, so the layout knows
    /// the layer the window is on.
    fn window_layer_known(&self, wid: WindowId) -> bool {
        self.layout_window_info(wid).is_some_and(|info| info.layer.is_some())
    }

    /// Decides the membership of windows the reactor sees for the first time,
    /// or decides them again when their layer became known. A window that the
    /// app reports in its own list is decided here even if the window server
    /// hasn't listed it.
    pub(super) fn decide_membership(&mut self, wids: &[WindowId]) {
        if !self.contexts_enabled() {
            return;
        }
        for wid in wids {
            self.pending_first_seen.remove(wid);
        }
        let own_pid = std::process::id() as pid_t;
        let windows: Vec<WindowDesc> = wids
            .iter()
            .filter(|wid| wid.pid != own_pid && self.apps.contains_key(&wid.pid))
            .filter(|&&wid| {
                self.layout_window_info(wid)
                    .is_some_and(|info| !self.layout.is_untracked(&info))
            })
            .filter_map(|&wid| self.window_desc(wid))
            .collect();
        if windows.is_empty() {
            return;
        }
        let changed = if self.startup_complete {
            self.windows_appeared(&windows)
        } else {
            let matches = self.contexts.rejoin_all(&windows, MatchPass::Arrival);
            let rejoined = matches.iter().filter(|found| !found.is_empty()).count();
            if rejoined > 0 {
                info!(rejoined, "Windows open at launch rejoined their contexts");
            }
            rejoined > 0
        };
        if changed {
            self.save_contexts();
        }
    }

    /// A new tab of a native tab group that the reactor knows joins the
    /// contexts of the group's main tab. The other windows are matched together
    /// against the member records, and each window that matches nothing joins
    /// the context its screen shows. Returns whether any window joined or
    /// rejoined a context.
    fn windows_appeared(&mut self, windows: &[WindowDesc]) -> bool {
        let new: Vec<WindowId> = windows.iter().map(|window| window.wid).collect();
        let mut changed = false;
        let mut by_context: Vec<(ContextKey, Vec<WindowDesc>)> = vec![];
        for window in windows {
            let main_tab =
                self.main_tab(window.wid).filter(|&main| !new.contains(&main)).or_else(|| {
                    let tabs = self.tabs_of(window.wid);
                    tabs.into_iter().find(|tab| !new.contains(tab))
                });
            if let Some(main_tab) = main_tab {
                changed |= self.join_tab_group(window, main_tab);
                continue;
            }
            let key = self.arrival_context(window.wid);
            match by_context.iter_mut().find(|(other, _)| *other == key) {
                Some((_, group)) => group.push(window.clone()),
                None => by_context.push((key, vec![window.clone()])),
            }
        }
        for (key, group) in by_context {
            let arrivals = self.contexts.windows_appeared(&group, key);
            for (window, arrival) in group.iter().zip(arrivals) {
                debug!(wid = ?window.wid, ?arrival, "A new window appeared");
                changed |= matches!(arrival, Arrival::Rejoined(_) | Arrival::Joined(_));
            }
        }
        changed
    }

    /// Whether the window server lists the window as visible and accessibility
    /// hasn't reported it closed. A window that is minimized, that is only on
    /// another Space, or that was closed with ⌘W is not on screen, and its
    /// frame is its last tile, which another window of the app can share.
    pub(super) fn window_on_screen(&self, wid: WindowId) -> bool {
        self.windows.get(&wid).is_some_and(|window| {
            window.window_server_id.is_some_and(|wsid| {
                self.visible_windows.contains(&wsid) && !self.hidden_windows.contains(&wsid)
            })
        })
    }

    /// The windows of the app that share the window's frame, the window first.
    /// Native tabs of one group are separate windows with one frame, and both
    /// are on screen. Parked windows share a corner without being tabs, and a
    /// window that isn't on screen is no tab of one that is.
    pub(super) fn tabs_of(&self, wid: WindowId) -> Vec<WindowId> {
        let Some(window) = self.windows.get(&wid) else {
            return vec![];
        };
        if self.parked.contains_key(&wid) {
            return vec![wid];
        }
        let key = Self::frame_key(&window.frame_monotonic);
        let mut tabs: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|&(&other, other_window)| {
                other != wid
                    && other.pid == wid.pid
                    && !self.parked.contains_key(&other)
                    && self.window_on_screen(other)
                    && Self::frame_key(&other_window.frame_monotonic) == key
            })
            .map(|(&other, _)| other)
            .collect();
        tabs.sort();
        tabs.insert(0, wid);
        tabs
    }

    /// The window whose membership decides the window's: the main tab of its
    /// tab group, or the window itself.
    pub(super) fn membership_window(&self, wid: WindowId) -> WindowId {
        self.main_tab(wid).unwrap_or(wid)
    }

    /// The main tab of the window's tab group: the app's main window when it is
    /// one of the group's tabs other than `wid`. Both must be on screen: the
    /// frame of a minimized window, a window on another Space, or a window
    /// closed with ⌘W is only its last tile, so another window of the app can
    /// share it without being one of its tabs.
    fn main_tab(&self, wid: WindowId) -> Option<WindowId> {
        let main = self.main_window_tracker.app_main_window(wid.pid)?;
        if main == wid || self.parked.contains_key(&wid) || self.parked.contains_key(&main) {
            return None;
        }
        if !self.window_on_screen(wid) || !self.window_on_screen(main) {
            return None;
        }
        let key = |wid| Some(Self::frame_key(&self.windows.get(&wid)?.frame_monotonic));
        (key(wid)? == key(main)?).then_some(main)
    }

    /// A new tab joins the contexts of its group's main tab, and is pinned when
    /// the main tab is. Returns whether it joined anything.
    fn join_tab_group(&mut self, tab: &WindowDesc, main_tab: WindowId) -> bool {
        let mut joined = false;
        for id in self.contexts.contexts_of(main_tab) {
            joined |= self.contexts.add_window(id, tab).unwrap_or(false);
        }
        if self.contexts.is_pinned(main_tab) {
            joined |= self.contexts.pin(tab);
        }
        debug!(wid = ?tab.wid, ?main_tab, joined, "A new tab joined its group's contexts");
        joined
    }

    /// The context a new window joins when it matches no record: the one the
    /// screen it appears on shows. A window on no managed screen joins the
    /// active context.
    fn arrival_context(&self, wid: WindowId) -> ContextKey {
        match self.layout_frame(wid).and_then(|frame| self.best_space_for_window(&frame)) {
            Some(space) => self.shown_context(space),
            None => self.contexts.active(),
        }
    }

    /// Whether a record of a window of `pid` has `link`'s kind: open, or closed
    /// and pending.
    fn has_records(&self, pid: pid_t, link: fn(RecordLink) -> Option<WindowId>) -> bool {
        self.contexts
            .contexts()
            .iter()
            .flat_map(|context| &context.members)
            .chain(self.contexts.pinned())
            .any(|record| link(record.link).is_some_and(|wid| wid.pid == pid))
    }

    /// The window's title changed. Its member records take the new title, so a
    /// window that appears with it after a relaunch can match them. A title
    /// change alone doesn't write `contexts.json`. With contexts off the title
    /// stays as the window was created: window rules never saw a change from
    /// a title before contexts existed, and R28 keeps it that way.
    pub(super) fn title_changed(&mut self, wid: WindowId, title: Secret<String>) {
        if !self.contexts_enabled() {
            return;
        }
        if self.windows.contains_key(&wid) {
            self.contexts.title_changed(wid, title.expose_secret());
        }
        match self.windows.get_mut(&wid) {
            Some(window) => window.title = title,
            None => debug!(?wid, "Title change of an unknown window"),
        }
    }

    /// A window closed. Its records wait, pending, until its app shows whether
    /// it quit. This keeps records current whether or not contexts are on, so
    /// that a record can't stay bound to a window that is gone.
    pub(super) fn window_closed(&mut self, wid: WindowId) {
        self.contexts.window_closed(wid);
    }

    /// The app quit. Its records stay, and keep the windows' last titles, so
    /// its windows can rejoin when it runs again. The contexts are saved when
    /// the app had records.
    pub(super) fn app_terminated(&mut self, pid: pid_t) {
        let had_records = self.has_records(pid, |link| match link {
            RecordLink::Live(wid) | RecordLink::Pending(wid) => Some(wid),
            RecordLink::Empty => None,
        });
        if had_records {
            self.contexts.app_terminated(pid);
            self.save_contexts();
        }
    }

    /// The app showed that it is still running, so its closed windows are gone
    /// for good, and their pending records go.
    pub(super) fn app_still_running(&mut self, pid: pid_t) {
        if !self.apps.contains_key(&pid) {
            return;
        }
        let pending = self.has_records(pid, |link| match link {
            RecordLink::Pending(wid) => Some(wid),
            RecordLink::Live(_) | RecordLink::Empty => None,
        });
        if pending {
            info!(
                pid,
                "Deleting the records of windows closed by an app that still runs"
            );
            self.contexts.app_still_running(pid);
            self.save_contexts();
        }
    }

    /// The windows that a membership command for `window` acts on: the window's
    /// native tab group. Fails when there is no window, or it is Sugarglider's
    /// own, untracked, or parked. The caller checks that contexts are on and
    /// that Sugarglider isn't quitting.
    fn command_windows(&self, window: Option<WindowId>) -> Result<Vec<WindowId>, String> {
        let wid = window.ok_or("No window has focus")?;
        let own_pid = std::process::id() as pid_t;
        let untracked =
            self.layout_window_info(wid).is_none_or(|info| self.layout.is_untracked(&info));
        if wid.pid == own_pid || untracked || self.parked.contains_key(&wid) {
            return Err("The focused window can't be in a context".to_string());
        }
        Ok(self.tabs_of(wid))
    }

    fn command_window_key(&self, wid: WindowId) -> ContextKey {
        self.layout_frame(wid)
            .and_then(|frame| self.best_screen_idx_for_window(&frame))
            .map_or_else(
                || self.screen_key(self.focused_screen_index()),
                |screen| self.screen_key(screen),
            )
    }

    /// Adds the window and its tabs to the context. They take effect at the
    /// next switch: until then the windows count as members of the active
    /// context, so they stay where they are.
    pub(super) fn add_window_to_context(
        &mut self,
        window: Option<WindowId>,
        reference: &ContextRef,
    ) -> Result<(), String> {
        let tabs = self.command_windows(window)?;
        let id = self.resolve_named(reference)?;
        for &tab in &tabs {
            if let Some(desc) = self.window_desc(tab) {
                _ = self.contexts.add_window(id, &desc);
                self.added_since_switch.insert(tab);
            }
        }
        info!(?tabs, ?id, "Added the window to a context");
        self.save_contexts();
        Ok(())
    }

    /// Moves the window and its tabs out of the active context and into the
    /// named one, at once. If they no longer show, they are parked.
    pub(super) fn move_window_to_context(
        &mut self,
        window: Option<WindowId>,
        reference: &ContextRef,
    ) -> Result<(), String> {
        let tabs = self.command_windows(window)?;
        let id = self.resolve_named(reference)?;
        let active = self.command_window_key(tabs[0]);
        for &tab in &tabs {
            if let Some(desc) = self.window_desc(tab) {
                _ = self.contexts.move_window(active, id, &desc);
                self.added_since_switch.remove(&tab);
            }
        }
        info!(?tabs, ?id, "Moved the window to a context");
        self.save_contexts();
        self.park_windows_that_left(&tabs)?;
        Ok(())
    }

    /// Removes the window and its tabs from the active context, at once. If
    /// they no longer show, they are parked.
    pub(super) fn remove_window_from_context(
        &mut self,
        window: Option<WindowId>,
    ) -> Result<(), String> {
        let tabs = self.command_windows(window)?;
        let ContextKey::Named(id) = self.command_window_key(tabs[0]) else {
            return Err("No named context is active to remove the window from".to_string());
        };
        for &tab in &tabs {
            _ = self.contexts.remove_window(id, tab);
            self.added_since_switch.remove(&tab);
        }
        info!(?tabs, ?id, "Removed the window from the active context");
        self.save_contexts();
        self.park_windows_that_left(&tabs)?;
        Ok(())
    }

    /// Pins the window and its tabs, which makes them members of every context,
    pub(super) fn toggle_window_pinned(&mut self, window: Option<WindowId>) -> Result<(), String> {
        let tabs = self.command_windows(window)?;
        let unpin = self.contexts.is_pinned(tabs[0]);
        for &tab in &tabs {
            if unpin {
                self.contexts.unpin(tab);
            } else if let Some(desc) = self.window_desc(tab) {
                self.contexts.pin(&desc);
            }
        }
        info!(?tabs, pinned = !unpin, "Toggled pinning the window");
        self.save_contexts();
        if unpin {
            self.park_windows_that_left(&tabs)?;
        }
        Ok(())
    }

    /// The windows a switcher command acts on: each window resolved to its
    /// native tab group (R36), without the windows that are pinned, which are
    /// members of every context already (R3), and without duplicates.
    pub(super) fn group_windows(&self, wids: &[WindowId]) -> Vec<WindowId> {
        let mut windows: Vec<WindowId> = vec![];
        for &wid in wids {
            for tab in self.tabs_of(self.membership_window(wid)) {
                if self.contexts.is_pinned(tab) || windows.contains(&tab) {
                    continue;
                }
                windows.push(tab);
            }
        }
        windows
    }

    /// The window a membership command acts on: the one it carries, or the
    /// focused window when it carries none. A command that carries a window
    /// never falls back to the focused one, because the switcher panel has
    /// key focus while it is open.
    pub(super) fn carried_window(&self, window: Option<WindowId>) -> Option<WindowId> {
        window.or_else(|| self.main_window())
    }

    /// Changes a context's members as the switcher's edit view asks: the
    /// records in `remove_records` go first (R23), then the windows in
    /// `remove` leave at once (R37), then the windows in `add` join for the
    /// next switch. A pinned window is left out of both lists.
    pub(super) fn edit_context(
        &mut self,
        reference: &ContextRef,
        add: &[WindowId],
        remove: &[WindowId],
        remove_records: &[RecordRef],
    ) -> Result<(), String> {
        let id = self.resolve_named(reference)?;
        self.remove_member_records(id, remove_records);
        let removed = self.group_windows(remove);
        for &wid in &removed {
            _ = self.contexts.remove_window(id, wid);
            self.added_since_switch.remove(&wid);
        }
        let added = self.group_windows(add);
        for &wid in &added {
            if let Some(desc) = self.window_desc(wid) {
                _ = self.contexts.add_window(id, &desc);
                self.added_since_switch.insert(wid);
            }
        }
        info!(
            ?id,
            added = added.len(),
            removed = removed.len(),
            "Edited a context"
        );
        self.save_contexts();
        self.park_windows_that_left(&removed)?;
        Ok(())
    }

    /// Removes the member records the switcher listed, from the highest index
    /// down, so that removing one record doesn't move the next one's index. A
    /// record that no longer matches its app and title, or that has an open
    /// window again, is skipped and logged: R23 changes the list while the
    /// panel is open.
    fn remove_member_records(&mut self, id: ContextId, refs: &[RecordRef]) {
        let mut highest_first: Vec<&RecordRef> = refs.iter().collect();
        highest_first.sort_by_key(|item| std::cmp::Reverse(item.record));
        for item in highest_first {
            let matches = self
                .contexts
                .get(id)
                .and_then(|context| context.members.get(item.record))
                .is_some_and(|record| {
                    record.window().is_none()
                        && app_name(record) == item.app
                        && record.title == item.title
                });
            if !matches {
                info!(
                    ?id,
                    record = item.record,
                    "Skipping a member record that changed before the edit"
                );
                continue;
            }
            _ = self.contexts.remove_record(Slot::Context(id), item.record);
        }
    }

    /// Parks the windows that left the active context and no longer show, with
    /// their journal entries written first, takes them out of the layout, and
    /// focuses the active context's most recently focused member.
    pub(super) fn park_windows_that_left(&mut self, wids: &[WindowId]) -> Result<(), String> {
        if !self.contexts_in_use() {
            return Ok(());
        }
        let spaces = self.shown_spaces(Apply::Again);
        let park: Vec<WindowId> = plan_switch(&self.switch_input(&spaces, None))
            .park
            .into_iter()
            .filter(|wid| wids.contains(wid))
            .collect();
        if park.is_empty() {
            return Ok(());
        }
        let parked = match self.journal_parking(&park) {
            Ok(parking) => self.move_to_corners(parking),
            Err(err) => {
                let reason = format!("Could not park a visible nonmember: {err}");
                self.abort_failed_parking(reason.clone());
                return Err(reason);
            }
        };
        let mut pids: Vec<pid_t> = parked.iter().map(|wid| wid.pid).collect();
        pids.sort();
        pids.dedup();
        for pid in pids {
            self.send_visible_windows_to_layout(pid);
        }
        let focus = plan_switch(&self.switch_input(&spaces, None)).focus;
        self.focus_after_parking(Default::default(), focus, false, &parked);
        Ok(())
    }

    /// Parks the windows of `pid` that must not show, with their journal
    /// entries written first. Sugarglider's own windows, untracked windows, and
    /// windows outside the visible-window set are left alone, and so is the
    /// main window, which has taken focus. Does nothing while contexts aren't
    /// in use and while quitting.
    pub(super) fn park_what_must_not_show(&mut self, pid: pid_t) {
        if !self.contexts_in_use() || self.pending_exit.is_some() {
            return;
        }
        let spaces = self.shown_spaces(Apply::Again);
        let main_window = self.main_window();
        let park: Vec<WindowId> = plan_switch(&self.switch_input(&spaces, None))
            .park
            .into_iter()
            .filter(|wid| wid.pid == pid && Some(*wid) != main_window)
            .collect();
        if park.is_empty() {
            return;
        }
        match self.journal_parking(&park) {
            Ok(parking) => {
                let parked = self.move_to_corners(parking);
                info!(?parked, "Parking windows that must not show");
            }
            Err(err) => {
                self.abort_failed_parking(format!("Could not park a visible nonmember: {err}"));
            }
        }
    }
}

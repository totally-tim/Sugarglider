// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Switching between contexts, the named window sets that each have their own
//! layout. A switch shows one context's windows and parks every other window.
//! The design is in `docs/specs/contexts.md`.

use std::io;
use std::time::{Instant, SystemTime};

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use tracing::{debug, error, info, warn};

use super::{ContextCommand, ContextRef, Event, Reactor, RecordRef, fit_frame_to_screen};
use crate::actor::app::{Request, WindowId, pid_t};
use crate::actor::contexts_snapshot::{CONTEXTS_OFF, app_name};
use crate::actor::contexts_store::{ContextsStore, Loaded, empty_contexts_after};
use crate::actor::layout::{ActiveContext, EventResponse, LayoutEvent};
use crate::model::contexts::{
    ContextError, ContextId, ContextKey, Contexts, MatchPass, Scope, Slot, SwitchInput, SwitchMove,
    SwitchPlan, SwitchScreen, SwitchWindow, WindowDesc, plan_switch, resolve,
};
use crate::sys::geometry::CGRectExt;
use crate::sys::screen::{ScreenId, SpaceId};
use crate::sys::window_server::WindowServerId;

/// Why a context command does nothing while a quit waits for parked windows.
const QUITTING: &str = "Sugarglider is quitting";

/// Why a switch or a new context does nothing while no screen shows a Space
/// that Sugarglider manages.
const NO_MANAGED_SPACE: &str = "No Space is managed right now";

/// A visible screen, with its Space and size, and the context it shows.
#[derive(Clone, Copy, Debug)]
pub(super) struct ShownSpace {
    screen: usize,
    space: SpaceId,
    size: CGSize,
    key: ContextKey,
}

/// Why contexts are applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Apply {
    /// The user switches to this context: every screen in `global` scope, or
    /// only `screen` in `per_screen` scope, which takes the context's
    /// members along from the other screens (R7, R8).
    Switch {
        target: ContextKey,
        screen: Option<usize>,
    },
    /// The active context is applied again. If the journal can't be written,
    /// the Spaces still show their contexts, and nothing is parked.
    Again,
}

enum ApplyError {
    BeforeSwitch(io::Error),
    ParkingAborted(String),
}

impl Reactor {
    pub(super) fn contexts_enabled(&self) -> bool {
        self.config.settings.experimental.contexts.enable
    }

    /// Whether a switch changes every screen or only the focused one (R7, R8).
    pub(super) fn scope(&self) -> Scope {
        self.config.settings.experimental.contexts.scope
    }

    /// Whether applying contexts can change anything. Without contexts and
    /// parked windows, Spaces are shown exactly as without the feature.
    pub(super) fn contexts_in_use(&self) -> bool {
        self.contexts_enabled()
            && (!self.contexts.contexts().is_empty()
                || self.contexts.active() != ContextKey::Everything
                || !self.parked.is_empty())
    }

    /// The context the Space shows when `active` is the active context. With
    /// contexts off, while quitting, on a Space about to be turned off, and in
    /// place of a context that doesn't exist, that is Everything.
    fn shown_with(&self, space: SpaceId, active: ContextKey) -> ContextKey {
        if !self.contexts_enabled()
            || self.pending_exit.is_some()
            || self.showing_everything.contains(&space)
        {
            return ContextKey::Everything;
        }
        if let ContextKey::Named(id) = active
            && self.contexts.get(id).is_none()
        {
            error!(
                ?active,
                "Showing Everything in place of a context that doesn't exist"
            );
            return ContextKey::Everything;
        }
        active
    }

    /// The context the screen at `screen` shows before the switch being
    /// applied: the global context in `global` scope, and the screen's own in
    /// `per_screen` scope.
    pub(super) fn screen_key(&self, screen: usize) -> ContextKey {
        match self.scope() {
            Scope::Global => self.contexts.active(),
            Scope::PerScreen => self
                .screens
                .get(screen)
                .map_or(ContextKey::Everything, |shown| self.contexts.active_on(shown.id)),
        }
    }

    /// The context the screen shows, or its own active context while it shows
    /// no Space.
    pub(super) fn screen_shown_key(&self, screen: usize) -> ContextKey {
        match self.screens.get(screen).and_then(|shown| shown.space) {
            Some(space) => self.shown_context(space),
            None => self.screen_key(screen),
        }
    }

    /// The context the Space shows. A Space that no visible screen shows
    /// falls back to the global context.
    pub(super) fn shown_context(&self, space: SpaceId) -> ContextKey {
        let key = self
            .screens
            .iter()
            .position(|screen| screen.space == Some(space))
            .map_or_else(|| self.contexts.active(), |screen| self.screen_key(screen));
        self.shown_with(space, key)
    }

    /// Whether the window is one the Space shows. Under Everything
    /// every window is; under a context only its members are.
    pub(super) fn shows_on(&self, space: SpaceId, wid: WindowId) -> bool {
        self.shows_under(self.shown_context(space), wid)
    }

    /// Whether the window shows when `key` is shown. A window added to a
    /// context since the last switch counts as a member of the context its
    /// screen shows until the next switch (R37). A tab shows with its group's
    /// main tab.
    pub(super) fn shows_under(&self, key: ContextKey, wid: WindowId) -> bool {
        let wid = self.membership_window(wid);
        match key {
            ContextKey::Everything => true,
            key => self.contexts.is_member(key, wid) || self.added_since_switch.contains(&wid),
        }
    }

    /// Whether the window may reach the layout the Space shows: be
    /// added to it, change Space in it, or take focus in it from the mouse.
    /// A parked window may not, whatever its geometry says. Under a context
    /// only the context's members may.
    pub(super) fn reaches_layout(&self, space: SpaceId, wid: WindowId) -> bool {
        self.membership_window(wid) == wid
            && !self.parked.contains_key(&wid)
            && self.shows_on(space, wid)
    }

    /// The visible screens and the contexts they show after `apply`. A
    /// switch that names a screen changes only that screen in `per_screen`
    /// scope; the others keep what they show.
    pub(super) fn shown_spaces(&self, apply: Apply) -> Vec<ShownSpace> {
        self.screens
            .iter()
            .enumerate()
            .filter_map(|(screen, info)| {
                let space = info.space?;
                let key = match apply {
                    Apply::Switch { target, screen: only }
                        if only.is_none_or(|only| only == screen) =>
                    {
                        target
                    }
                    Apply::Switch { .. } | Apply::Again => self.screen_key(screen),
                };
                Some(ShownSpace {
                    screen,
                    space,
                    size: info.frame.size,
                    key: self.shown_with(space, key),
                })
            })
            .collect()
    }

    /// The place of the screen a switch changes in `spaces`, which is what
    /// the plan names. `None` when the switch changes every screen, and also
    /// when the changed screen shows no Space.
    fn only_position(apply: Apply, spaces: &[ShownSpace]) -> Option<usize> {
        let Apply::Switch { screen: Some(screen), .. } = apply else {
            return None;
        };
        spaces.iter().position(|shown| shown.screen == screen)
    }

    /// An active context cannot claim that every nonmember is parked while AX
    /// omits one that WindowServer lists. A passive reapply falls back to
    /// Everything when the sources disagree, including a stale Cmd+W row.
    fn check_on_screen_nonmembers(
        &self,
        spaces: &[ShownSpace],
        only: Option<usize>,
    ) -> io::Result<()> {
        for wsid in &self.on_screen_windows {
            let Some(info) = self.window_server_info.get(wsid) else {
                continue;
            };
            if info.layer != 0 || info.pid == std::process::id() as pid_t {
                continue;
            }
            for (slot, shown) in spaces.iter().enumerate() {
                if only.is_some_and(|only| only != slot)
                    || shown.key == ContextKey::Everything
                    || self.screens[shown.screen].bounds.intersection(&info.frame).area() <= 0.0
                {
                    continue;
                }
                match self.window_ids.get(wsid).copied() {
                    // An explicit destroy is stronger evidence than a stale
                    // WindowServer snapshot.
                    Some(wid) if wid.pid == info.pid && !self.windows.contains_key(&wid) => {}
                    Some(wid)
                        if wid.pid == info.pid
                            && self.windows[&wid].window_server_id == Some(*wsid)
                            && (self.parked.contains_key(&wid)
                                || !self.hidden_windows.contains(wsid)
                                || self.shows_under(shown.key, wid)
                                || self
                                    .layout_window_info(wid)
                                    .is_some_and(|info| self.layout.is_untracked(&info))) => {}
                    _ => {
                        return Err(io::Error::other(format!(
                            "WindowServer still lists window {wsid:?} from app {}, but AX has not confirmed it",
                            info.pid
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// The context and its open members, as the layout needs it.
    fn active_context(&self, key: ContextKey) -> ActiveContext {
        if key == ContextKey::Everything {
            return ActiveContext::EVERYTHING;
        }
        ActiveContext {
            key,
            members: self
                .windows
                .keys()
                .copied()
                .filter(|&wid| self.contexts.is_member(key, wid))
                .collect(),
        }
    }

    /// Makes each Space's context the one its layout shows, and returns the
    /// layout's responses.
    fn expose(&mut self, spaces: &[ShownSpace]) -> Option<EventResponse> {
        spaces
            .iter()
            .map(|shown| {
                let context = self.active_context(shown.key);
                let event = LayoutEvent::SpaceExposed(shown.space, shown.size, context);
                self.layout.handle_event(event)
            })
            .reduce(EventResponse::coalesce)
    }

    /// Whether the window server's list of visible windows names at least one
    /// window the reactor knows, if it knows any. Otherwise the list is taken
    /// to be incomplete, as it is right after the login window.
    fn lists_known_windows(&self) -> bool {
        self.window_ids.is_empty()
            || self.visible_windows.iter().any(|wsid| self.window_ids.contains_key(wsid))
    }

    /// Shows each visible Space's context in the layout, and applies the
    /// active context again when contexts are in use. Moves focus away from
    /// a window the apply parks, using the event's visible window order to
    /// avoid redundant raises.
    ///
    /// When the list of visible windows is incomplete, the Spaces only show
    /// their contexts, so that no window loses its tile.
    pub(super) fn show_visible_spaces(&mut self, visible_window_order: Option<&[WindowServerId]>) {
        if self.contexts_in_use() && self.lists_known_windows() {
            if let Ok((plan, response)) = self.apply(Apply::Again) {
                let response = response.map(|response| match visible_window_order {
                    Some(order) => self.filter_response(response, order),
                    None => response,
                });
                self.finish_apply(plan, response);
            }
            return;
        }
        let spaces = self.shown_spaces(Apply::Again);
        if let Some(response) = self.expose(&spaces) {
            let response = match visible_window_order {
                Some(order) => self.filter_response(response, order),
                None => response,
            };
            self.handle_layout_response(response);
        }
    }

    /// Applies the context each visible Space shows again, as a switch does,
    /// and runs the switch's focus step when the apply parked the window that
    /// has the focus: its context's most recently focused member takes the
    /// focus, or Finder is activated, so that keystrokes don't go to a parked
    /// window. Used for applies that have no event snapshot of window order.
    pub(super) fn apply_again_focusing_parked_main(&mut self) {
        self.show_visible_spaces(None);
    }

    /// Handles the layout's response to an apply. When the apply parked the
    /// window that has the focus, the switch's focus step runs, so that
    /// keystrokes don't go to a parked window.
    fn finish_apply(&mut self, plan: SwitchPlan, response: Option<EventResponse>) {
        let parked: Vec<WindowId> =
            plan.park.iter().copied().filter(|wid| self.parked.contains_key(wid)).collect();
        let main_parked = self.main_window().is_some_and(|main| parked.contains(&main));
        if !main_parked {
            if let Some(response) = response {
                self.handle_layout_response(response);
            }
            return;
        }
        info!(?parked, "The apply parked the focused window; moving the focus");
        self.focus_after_parking(response.unwrap_or_default(), plan.focus, false, &parked);
    }

    /// Describes the windows on the visible screens for `plan_switch`. A
    /// window on no screen counts as on the first visible screen. So does a
    /// parked window on a screen that shows a Space Sugarglider doesn't
    /// manage, so that the plan puts it back when it must show. The other
    /// windows on such a screen are left out. `only` names the screen a
    /// `per_screen` switch changes, as a place in `spaces`.
    pub(super) fn switch_input(&self, spaces: &[ShownSpace], only: Option<usize>) -> SwitchInput {
        let mut input = SwitchInput {
            screens: spaces
                .iter()
                .map(|shown| SwitchScreen {
                    active: shown.key,
                    windows: vec![],
                })
                .collect(),
            only,
        };
        if spaces.is_empty() {
            return input;
        }
        let own_pid = std::process::id() as pid_t;
        let mut wids: Vec<WindowId> = self
            .windows
            .keys()
            .copied()
            .filter(|wid| self.apps.contains_key(&wid.pid))
            .collect();
        wids.sort();
        for wid in wids {
            let Some(info) = self.layout_window_info(wid) else {
                continue;
            };
            let parked = self.parked.contains_key(&wid);
            let mut unmanaged = false;
            let slot = match self.best_screen_idx_for_window(&info.frame) {
                Some(screen) => match spaces.iter().position(|shown| shown.screen == screen) {
                    Some(slot) => slot,
                    None if parked => {
                        unmanaged = true;
                        0
                    }
                    None => continue,
                },
                None => 0,
            };
            let visible = !unmanaged
                && self.windows[&wid]
                    .window_server_id
                    .is_some_and(|wsid| self.visible_windows.contains(&wsid));
            // A tab has the membership of its group's main tab.
            let decides = self.membership_window(wid);
            let mut window = SwitchWindow {
                wid,
                last_focus: self.contexts.last_focus(wid),
                ..self.contexts.switch_window(decides)
            };
            if self.added_since_switch.contains(&decides) {
                // It counts as a member of the context the screen it is on
                // shows (R37).
                match spaces[slot].key {
                    ContextKey::Named(id) if !window.contexts.contains(&id) => {
                        window.contexts.push(id)
                    }
                    ContextKey::Unsorted => window.contexts.clear(),
                    _ => {}
                }
            }
            window.parked = parked;
            window.own = wid.pid == own_pid;
            window.untracked = self.layout.is_untracked(&info);
            // Windows on Spaces Sugarglider doesn't manage count as
            // invisible too, and the plan never parks them.
            window.invisible = !visible || self.membership_window(wid) != wid;
            input.screens[slot].windows.push(window);
        }
        input
    }

    /// Applies contexts to the visible Spaces in the order a switch takes:
    /// the journal entries of the windows it parks, then the layouts, then
    /// the members put back and laid out, then the parking. A per-screen
    /// switch moves the target's members on other screens over first. Parked
    /// windows that are no longer in their corners, for example because their
    /// app moved them, are parked again, except while quitting and with
    /// contexts off. Returns the plan and the layout's response to the
    /// exposure, which the caller handles.
    fn apply(&mut self, apply: Apply) -> Result<(SwitchPlan, Option<EventResponse>), ApplyError> {
        let spaces = self.shown_spaces(apply);
        let only = Self::only_position(apply, &spaces);
        if self.unidentified_native_tabs()
            && spaces.iter().any(|shown| shown.key != ContextKey::Everything)
        {
            let reason = Self::unidentified_tabs_message().to_string();
            self.abort_failed_parking(reason.clone());
            return Err(ApplyError::ParkingAborted(reason));
        }
        let plan = plan_switch(&self.switch_input(&spaces, only));
        let parking = match self
            .check_on_screen_nonmembers(&spaces, only)
            .and_then(|()| self.journal_parking(&plan.park))
        {
            Ok(parking) => parking,
            Err(err) if matches!(apply, Apply::Switch { .. }) => {
                return Err(ApplyError::BeforeSwitch(err));
            }
            Err(err) => {
                let reason = format!("Could not park a visible nonmember: {err}");
                self.abort_failed_parking(reason.clone());
                return Err(ApplyError::ParkingAborted(reason));
            }
        };
        if let Apply::Switch { target, screen } = apply {
            let shown = screen.and_then(|screen| self.screens.get(screen)).map(|shown| shown.id);
            let result = match shown {
                Some(screen) => self.contexts.switch_to_on(screen, target),
                None => self.contexts.switch_to(target),
            };
            if let Err(err) = result {
                error!(?target, "Could not switch: {err}");
            }
        }
        let response = self.expose(&spaces);
        // Windows put back still count as parked here, so that the layout
        // sees them at their frames from before parking, and never as tabs
        // that share a corner.
        let mut pids: Vec<pid_t> = self.apps.keys().copied().collect();
        pids.sort();
        for pid in pids {
            self.send_visible_windows_to_layout(pid);
        }
        let released = self.release_parked(&plan.unpark);
        self.move_windows(&plan.moves, &spaces);
        self.put_back_unplaced(&released);
        self.update_layout(&[], true);
        self.move_to_corners(parking);
        if self.contexts_enabled() && self.pending_exit.is_none() {
            self.repark_moved_windows().map_err(ApplyError::ParkingAborted)?;
        }
        Ok((plan, response))
    }

    /// Moves the windows a per-screen switch takes with it to their new
    /// screen (R8, R9). A window keeps the place it had in each layout of the
    /// Space it leaves, so it returns there when it comes back. Until its
    /// frame lands on the new screen, the layout it left doesn't take it
    /// back.
    fn move_windows(&mut self, moves: &[SwitchMove], spaces: &[ShownSpace]) {
        for moved in moves {
            let Some(target) = spaces.get(moved.screen) else {
                continue;
            };
            let Some(from) = self
                .layout_frame(moved.wid)
                .and_then(|frame| self.best_space_for_window(&frame))
            else {
                continue;
            };
            if from == target.space {
                continue;
            }
            self.layout.move_window_to_space(moved.wid, from, target.space);
            self.moving_away.insert(moved.wid, from);
            if self.layout.is_floating_window(moved.wid) {
                self.place_floating_on_screen(moved.wid, target.screen);
            }
        }
    }

    /// Puts a floating window in the middle of the screen it moved to.
    fn place_floating_on_screen(&mut self, wid: WindowId, screen: usize) {
        let Some(screen) = self.screens.get(screen) else { return };
        let Some(window) = self.windows.get(&wid) else { return };
        let size = window.frame_monotonic.size;
        let frame = CGRect::new(
            CGPoint::new(
                screen.frame.origin.x + (screen.frame.size.width - size.width) / 2.0,
                screen.frame.origin.y + (screen.frame.size.height - size.height) / 2.0,
            ),
            size,
        );
        let frame = fit_frame_to_screen(frame, CGSize::new(0.0, 0.0), screen.frame);
        self.pending_frame_overrides.insert(wid, frame);
    }

    /// Switches to `target`: on every screen in `global` scope, and on the
    /// focused screen in `per_screen` scope. The switch focuses the most
    /// recently focused window that shows. If the journal can't be written,
    /// the old context stays.
    pub(super) fn switch_context(&mut self, target: ContextKey) -> Result<(), String> {
        self.switch_context_on(self.switch_screen(), target, None)
    }

    /// The screen a per-screen switch changes: the focused one (R8).
    fn switch_screen(&self) -> Option<usize> {
        match self.scope() {
            Scope::Global => None,
            Scope::PerScreen => Some(self.focused_screen_index()),
        }
    }

    /// The screen the focused window is on, or the first screen when that is
    /// unknown.
    pub(super) fn focused_screen_index(&self) -> usize {
        self.active_screen_idx
            .map_or(0, usize::from)
            .min(self.screens.len().saturating_sub(1))
    }

    /// Switches to `target` on `screen` in per-screen scope, or on every
    /// screen in global scope. `focused` is the window whose focus started
    /// the switch; otherwise the most recent member takes focus. Finder is
    /// activated when no window can take focus.
    pub(super) fn switch_context_on(
        &mut self,
        screen: Option<usize>,
        target: ContextKey,
        focused: Option<WindowId>,
    ) -> Result<(), String> {
        if !self.contexts_enabled() {
            debug!(?target, "Ignoring a context switch while contexts are off");
            return Err(CONTEXTS_OFF.to_string());
        }
        if self.pending_exit.is_some() {
            info!(?target, "Ignoring a context switch while quitting");
            return Err(QUITTING.to_string());
        }
        if let ContextKey::Named(id) = target
            && self.contexts.get(id).is_none()
        {
            warn!(?target, "Ignoring a switch to a context that doesn't exist");
            return Err(ContextError::NoSuchContext.to_string());
        }
        let start = Instant::now();
        self.layout.cancel_interactive_state();
        self.in_drag = false;
        self.resizing_window = None;
        self.title_bar_drag = None;
        // A switch is a fresh start for the limit on parking windows again.
        self.repark_counts.clear();
        let added = std::mem::take(&mut self.added_since_switch);
        let contexts = self.contexts.clone();
        self.rejoin_for_switch(target);
        match self.apply(Apply::Switch { target, screen }) {
            Ok((plan, response)) => {
                self.hide_context_switcher();
                self.save_contexts();
                let mut parked: Vec<WindowId> = self.pending_parking_writes().into_keys().collect();
                parked.sort();
                let response = response.unwrap_or_default();
                match focused {
                    Some(focused) => {
                        self.focus_after_parking(response, Some(focused), true, &parked);
                        self.select_in_layout(focused);
                        self.contexts.window_focused(focused);
                    }
                    None => self.focus_after_parking(response, plan.focus, false, &parked),
                }
                info!(
                    ?target,
                    ?screen,
                    parked = plan.park.len(),
                    put_back = plan.unpark.len(),
                    moved = plan.moves.len(),
                    elapsed = ?start.elapsed(),
                    "Switched context"
                );
                Ok(())
            }
            Err(ApplyError::BeforeSwitch(err)) => {
                self.added_since_switch = added;
                self.contexts = contexts;
                let reason = format!("Could not prepare parking, so the context stays: {err}");
                error!(?target, "{reason}");
                Err(reason)
            }
            Err(ApplyError::ParkingAborted(reason)) => Err(reason),
        }
    }

    /// Applies the context each visible Space shows again, and handles the
    /// layout's response.
    pub(super) fn apply_again(&mut self) {
        if let Ok((_, Some(response))) = self.apply(Apply::Again) {
            self.handle_layout_response(response);
        }
    }

    /// Shows every window on the visible ones of `spaces`, which Sugarglider
    /// is about to stop managing. The active context doesn't change, and the
    /// next space change applies it again.
    pub(super) fn show_everything_on(&mut self, spaces: &[SpaceId]) {
        self.hide_context_switcher();
        if !self.contexts_in_use() {
            return;
        }
        let visible: Vec<SpaceId> = self
            .screens
            .iter()
            .filter_map(|screen| screen.space)
            .filter(|space| spaces.contains(space))
            .collect();
        if visible.is_empty() {
            return;
        }
        info!(?visible, "Showing every window before Spaces are turned off");
        self.showing_everything.extend(visible);
        self.apply_again();
    }

    /// Runs a context command from a key binding or the menu, and logs why
    /// it did nothing.
    pub(super) fn handle_context_command(&mut self, command: ContextCommand) {
        if let Err(reason) = self.run_context_command(command) {
            info!("The context command did nothing: {reason}");
        }
    }

    /// Runs a context command, or returns why it can't run. The command
    /// names its context as the user wrote it, and it is resolved here,
    /// against the reactor's own state. A switch or a new context does
    /// nothing while no Space is managed, because it would apply to no
    /// Space now and change what shows when a Space is managed again.
    pub(super) fn run_context_command(&mut self, command: ContextCommand) -> Result<(), String> {
        if !self.contexts_enabled() {
            return Err(CONTEXTS_OFF.to_string());
        }
        if self.pending_exit.is_some() {
            return Err(QUITTING.to_string());
        }
        let opens = matches!(command, ContextCommand::OpenContextSwitcher);
        let switches = matches!(
            command,
            ContextCommand::SwitchContext(_)
                | ContextCommand::ShowEverything
                | ContextCommand::PreviousContext
                | ContextCommand::CreateContext(_)
                | ContextCommand::CreateContextFromWindows { .. }
                | ContextCommand::OpenContextSwitcher
        );
        if switches && self.screens.iter().all(|screen| screen.space.is_none()) {
            return Err(NO_MANAGED_SPACE.to_string());
        }
        if !self.startup_complete
            && self.scope() == Scope::Global
            && self.contexts.has_screen_actives()
        {
            if self.current_main_screen_index().is_none() {
                return Err("Contexts are waiting for the focused screen at startup".to_string());
            }
            self.update_active_screen();
        }
        self.reconcile_cold_scope();
        let result = match command {
            ContextCommand::OpenContextSwitcher => self.open_context_switcher(),
            ContextCommand::SwitchContext(reference) => {
                let key = self.resolve(&reference).map_err(|err| err.to_string())?;
                self.switch_context(key)
            }
            ContextCommand::ShowEverything => self.switch_context(ContextKey::Everything),
            ContextCommand::PreviousContext => {
                let key = match self.scope() {
                    Scope::Global => self.contexts.previous(),
                    Scope::PerScreen => {
                        let screen = self.focused_screen_index();
                        self.screens
                            .get(screen)
                            .and_then(|shown| self.contexts.previous_on(shown.id))
                    }
                };
                let key = key.ok_or("There is no previous context")?;
                self.switch_context(key)
            }
            ContextCommand::AddWindowToContext(reference) => {
                self.add_window_to_context(self.main_window(), &reference)
            }
            ContextCommand::MoveWindowToContext(reference) => {
                self.move_window_to_context(self.main_window(), &reference)
            }
            ContextCommand::RemoveWindowFromContext => {
                self.remove_window_from_context(self.main_window())
            }
            ContextCommand::ToggleWindowPinned => self.toggle_window_pinned(self.main_window()),
            ContextCommand::CreateContext(name) => self.create_context(&name),
            ContextCommand::AddWindow { window, context } => {
                self.add_window_to_context(self.carried_window(window), &context)
            }
            ContextCommand::MoveWindow { window, context } => {
                self.move_window_to_context(self.carried_window(window), &context)
            }
            ContextCommand::TogglePinned { window } => {
                self.toggle_window_pinned(self.carried_window(window))
            }
            ContextCommand::CreateContextFromWindows { name, windows } => {
                self.create_context_from_windows(&name, &windows)
            }
            ContextCommand::EditContext {
                context,
                add,
                remove,
                remove_records,
            } => self.edit_context(&context, &add, &remove, &remove_records),
            ContextCommand::RenameContext { context, name } => {
                let id =
                    self.resolve_named_context(&context, "Only a named context can be renamed")?;
                self.contexts.rename(id, &name).map_err(|err| err.to_string())?;
                info!(?id, name, "Renamed a context");
                self.save_contexts();
                Ok(())
            }
            ContextCommand::SetContextNumber { context, number } => {
                let id =
                    self.resolve_named_context(&context, "Only a named context can be numbered")?;
                self.contexts.set_number(id, Some(number)).map_err(|err| err.to_string())?;
                info!(?id, number, "Gave a context a number");
                self.save_contexts();
                Ok(())
            }
            ContextCommand::DeleteContext(context) => {
                let id =
                    self.resolve_named_context(&context, "Only a named context can be deleted")?;
                self.delete_context(id).map_err(|err| err.to_string())?;
                info!(?id, "Deleted a context");
                Ok(())
            }
            ContextCommand::EditContextMembers {
                context,
                add,
                remove,
                remove_records,
            } => {
                let id =
                    self.resolve_named_context(&context, "Only a named context can be edited")?;
                self.edit_context_members(id, &add, &remove, &remove_records)
            }
            ContextCommand::RemoveRecord { context, record } => {
                let id = self
                    .resolve_named_context(&context, "Only a named context has member records")?;
                self.remove_record(id, &record)
            }
        };
        if result.is_ok() && !opens {
            self.hide_context_switcher();
        }
        result
    }

    /// The named context that a command that changes contexts names, or
    /// `cannot` when the command names Everything or Unsorted.
    fn resolve_named_context(
        &self,
        reference: &ContextRef,
        cannot: &str,
    ) -> Result<ContextId, String> {
        match self.resolve(reference).map_err(|err| err.to_string())? {
            ContextKey::Named(id) => Ok(id),
            ContextKey::Everything | ContextKey::Unsorted => Err(cannot.to_string()),
        }
    }

    /// Removes the member record that `item` names, whose window is gone. A
    /// record whose window is open is left alone:
    /// `remove_window_from_context` is how a window leaves a context. The
    /// record must still have the app and title the client read: the list
    /// can shift between the client's `context list` and this command, and
    /// an index alone would then remove another record.
    fn remove_record(&mut self, id: ContextId, item: &RecordRef) -> Result<(), String> {
        let Some(record) =
            self.contexts.get(id).and_then(|context| context.members.get(item.record))
        else {
            return Err(ContextError::NoSuchRecord.to_string());
        };
        if app_name(record) != item.app || record.title != item.title {
            return Err(
                "The member record changed since it was listed; list the contexts again"
                    .to_string(),
            );
        }
        if record.window().is_some() {
            return Err("The member's window is open; remove the window instead".to_string());
        }
        let removed = self
            .contexts
            .remove_record(Slot::Context(id), item.record)
            .map_err(|err| err.to_string())?;
        info!(?id, record = item.record, ?removed, "Removed a member record");
        self.save_contexts();
        Ok(())
    }

    /// Changes a context's members. The records in `remove_records` go
    /// first, from the highest index down, and only while the index still
    /// names a record with no open window and the same app and title. Then
    /// the windows in `remove` leave at once, as R37 says, and the windows
    /// in `add` join for the next switch. Pinned windows are members of
    /// every context already, so they get no record and are left alone (R3).
    fn edit_context_members(
        &mut self,
        id: ContextId,
        add: &[WindowId],
        remove: &[WindowId],
        remove_records: &[RecordRef],
    ) -> Result<(), String> {
        let mut records: Vec<&RecordRef> = remove_records.iter().collect();
        records.sort_by_key(|item| std::cmp::Reverse(item.record));
        for item in records {
            if !self.remove_record_matching(id, item) {
                warn!(
                    ?id,
                    record = item.record,
                    "Skipping a member record that changed since it was listed"
                );
            }
        }
        let mut removed: Vec<WindowId> = Vec::new();
        for &window in remove {
            for tab in self.edit_windows(window) {
                if removed.contains(&tab) {
                    continue;
                }
                removed.push(tab);
                _ = self.contexts.remove_window(id, tab);
                self.added_since_switch.remove(&tab);
            }
        }
        let mut added: Vec<WindowId> = Vec::new();
        for &window in add {
            for tab in self.edit_windows(window) {
                if added.contains(&tab) {
                    continue;
                }
                if let Some(desc) = self.window_desc(tab) {
                    _ = self.contexts.add_window(id, &desc);
                    self.added_since_switch.insert(tab);
                    added.push(tab);
                }
            }
        }
        info!(
            ?id,
            added = added.len(),
            removed = removed.len(),
            "Edited a context's members"
        );
        self.save_contexts();
        if !removed.is_empty() {
            self.park_windows_that_left(&removed)?;
        }
        Ok(())
    }

    /// Removes the record at the index of `item` while it still has no open
    /// window and the same app and title. Returns whether it was removed.
    fn remove_record_matching(&mut self, id: ContextId, item: &RecordRef) -> bool {
        let matches = self
            .contexts
            .get(id)
            .and_then(|context| context.members.get(item.record))
            .is_some_and(|record| {
                record.window().is_none()
                    && app_name(record) == item.app
                    && record.title == item.title
            });
        matches && self.contexts.remove_record(Slot::Context(id), item.record).is_ok()
    }

    /// The windows that an edit's `add` or `remove` applies to: the
    /// window's native tab group (R36), without pinned windows (R3), and
    /// without windows the reactor or the layout doesn't track.
    fn edit_windows(&self, window: WindowId) -> Vec<WindowId> {
        self.tabs_of(self.membership_window(window))
            .into_iter()
            .filter(|&tab| {
                !self.contexts.is_pinned(tab)
                    && self.window_desc(tab).is_some()
                    && self
                        .layout_window_info(tab)
                        .is_some_and(|info| !self.layout.is_untracked(&info))
            })
            .collect()
    }

    /// The named context that a command names. Everything and Unsorted
    /// can't take a window, so naming them fails too.
    pub(super) fn resolve_named(&self, reference: &ContextRef) -> Result<ContextId, String> {
        match self.resolve(reference).map_err(|err| err.to_string())? {
            ContextKey::Named(id) => Ok(id),
            ContextKey::Everything | ContextKey::Unsorted => {
                Err("Only a named context can take a window".to_string())
            }
        }
    }

    /// The entry that a command names, as `model::contexts::resolve` finds
    /// it.
    pub(super) fn resolve(&self, reference: &ContextRef) -> Result<ContextKey, ContextError> {
        resolve(reference.query(), &self.contexts, self.lists_unsorted())
    }

    /// The unsorted windows: the windows on the visible Spaces that are in
    /// no named context and not pinned. Sugarglider's own windows and
    /// windows the layout doesn't track don't count. Before the first
    /// context exists there are none, and only Everything is listed.
    pub(super) fn unsorted_windows(&self) -> Vec<WindowId> {
        if self.contexts.contexts().is_empty() {
            return Vec::new();
        }
        self.windows_on_visible_spaces(|wid| self.contexts.is_unsorted(wid))
    }

    /// Whether Unsorted is listed among the contexts to switch to: while it
    /// has unsorted windows. The snapshot publishes this for the menu, the
    /// command line, and the switcher.
    pub(super) fn lists_unsorted(&self) -> bool {
        !self.unsorted_windows().is_empty()
    }

    /// Applies the active context after contexts were turned on, or shows
    /// every window after they were turned off. Contexts that were never
    /// read are read first.
    pub(super) fn contexts_turned_on_or_off(&mut self, previous_scope: Option<Scope>) {
        // Titles reach the window rules only while contexts are on, so that
        // with contexts off the rules see the same titles as before contexts
        // existed.
        for app in self.apps.values() {
            _ = app.handle.send(Request::TrackTitles(self.contexts_enabled()));
        }
        if self.contexts_enabled() {
            info!("Contexts are on");
            if self.contexts_unread {
                self.read_contexts_after_reload(SystemTime::now(), previous_scope);
            } else {
                self.rejoin_every_window();
            }
            if !self.contexts_in_use() {
                return;
            }
        } else {
            info!("Contexts are off; showing every window");
            self.hide_context_switcher();
            // A window that waits for the window server's list is not new when
            // contexts come back: it was found while they were off.
            self.pending_first_seen.clear();
        }
        self.apply_again_focusing_parked_main();
        if !self.contexts_enabled() {
            let rest: Vec<WindowId> = self.parked.keys().copied().collect();
            if !rest.is_empty() {
                self.unpark_windows(&rest);
            }
        }
    }

    /// Reconciles the active contexts after a change of scope (R11), even
    /// while contexts are disabled. Going from `global` to `per_screen`
    /// gives every screen the global context. Going the other way makes the
    /// focused screen's context the global one, and the other screens follow
    /// it.
    pub(super) fn scope_changed(&mut self, from: Scope) {
        match (from, self.scope()) {
            (Scope::Global, Scope::PerScreen) => {
                if self.contexts.has_screen_actives() {
                    return;
                }
                let key = self.contexts.active();
                let screens: Vec<ScreenId> = self.screens.iter().map(|screen| screen.id).collect();
                info!(?key, screens = screens.len(), "Changing to per-screen scope");
                self.contexts.set_screen_actives(screens, key);
            }
            (Scope::PerScreen, Scope::Global) => {
                if !self.contexts.has_screen_actives() {
                    return;
                }
                let screen = self.focused_screen_index();
                let key = self
                    .screens
                    .get(screen)
                    .map_or(self.contexts.active(), |shown| self.contexts.active_on(shown.id));
                info!(?key, "Changing to global scope");
                self.contexts.forget_screen_actives();
                if let Err(err) = self.contexts.switch_to(key) {
                    error!(?key, "Could not make the focused screen's context global: {err}");
                }
            }
            _ => return,
        }
        self.save_contexts();
    }

    /// Deletes a context. Its windows stay open, and the ones that were only
    /// in it become unsorted. The active context is applied again first, so
    /// that they show when Unsorted is active, and then the context's
    /// layouts go.
    pub(super) fn delete_context(&mut self, id: ContextId) -> Result<(), ContextError> {
        self.contexts.delete(id)?;
        self.apply_again();
        self.layout.remove_context_layouts(id);
        if self.contexts_enabled() {
            self.save_contexts();
        }
        Ok(())
    }

    /// Writes `contexts.json`. A failure is logged.
    pub(super) fn save_contexts(&self) {
        if let Err(err) = self.contexts_store.save(&self.contexts, self.boot_id.as_deref()) {
            error!("Could not write the contexts: {err}");
        }
    }

    /// The window as member records describe it.
    pub(super) fn window_desc(&self, wid: WindowId) -> Option<WindowDesc> {
        let window = self.windows.get(&wid)?;
        let app = self.apps.get(&wid.pid);
        Some(WindowDesc {
            wid,
            bundle_id: app.and_then(|app| app.info.bundle_id.clone()),
            app_name: app.and_then(|app| app.info.localized_name.clone()),
            title: window.title.expose_secret().clone(),
            window_server_id: window.window_server_id,
        })
    }

    /// Binds windows that the reactor finds to the empty member records they
    /// match, so they rejoin the contexts that hold those records. Returns
    /// whether any window rejoined.
    pub(super) fn rejoin_windows(&mut self, wids: &[WindowId]) -> bool {
        if !self.contexts_enabled() {
            return false;
        }
        let windows: Vec<WindowDesc> =
            wids.iter().filter_map(|&wid| self.window_desc(wid)).collect();
        if windows.is_empty() {
            return false;
        }
        let matches = self.contexts.rejoin_all(&windows, MatchPass::Arrival);
        let rejoined = matches.iter().filter(|found| !found.is_empty()).count();
        if rejoined > 0 {
            info!(rejoined, "Windows rejoined their contexts");
        }
        rejoined > 0
    }

    /// Matches the windows of the running apps against the empty member
    /// records, as a switch to `target` does: a window that matches no
    /// record by its window server id or its title, and is in no context,
    /// can fill an empty record of the target for its app. Only windows on
    /// screen take a record: a window closed with ⌘W that the app never
    /// reported destroyed still has its last frame and no live window of
    /// its own.
    fn rejoin_for_switch(&mut self, target: ContextKey) {
        let own_pid = std::process::id() as pid_t;
        let mut wids: Vec<WindowId> = self
            .windows
            .keys()
            .copied()
            .filter(|wid| wid.pid != own_pid && self.apps.contains_key(&wid.pid))
            .filter(|&wid| self.window_on_screen(wid))
            .filter(|&wid| {
                self.layout_window_info(wid)
                    .is_some_and(|info| !self.layout.is_untracked(&info))
            })
            .collect();
        wids.sort();
        let windows: Vec<WindowDesc> =
            wids.iter().filter_map(|&wid| self.window_desc(wid)).collect();
        let matches = self.contexts.rejoin_all(&windows, MatchPass::Switch { target });
        let rejoined = matches.iter().filter(|found| !found.is_empty()).count();
        if rejoined > 0 {
            info!(rejoined, ?target, "Windows filled empty records for the switch");
        }
    }

    /// Sets `store` as the place the contexts are read from and saved to,
    /// in the boot of the Mac that `boot_id` names. If contexts are on, the
    /// contexts are read now at `now`. Otherwise they are read when contexts
    /// are turned on. Until then the file is neither read nor changed, and
    /// the layouts of the contexts it names stay.
    pub(super) fn open_contexts(
        &mut self,
        store: ContextsStore,
        boot_id: Option<String>,
        now: SystemTime,
    ) {
        self.contexts_store = store;
        self.boot_id = boot_id;
        self.contexts_unread = true;
        if self.contexts_enabled() {
            self.read_contexts(now);
        }
    }

    /// Reads the contexts. A file that can't be read is moved aside with the
    /// time `now` in its name.
    ///
    /// Window server ids are valid only within one boot of the Mac, so the
    /// contexts forget their saved ids when the boot that saved them isn't
    /// the current one. Once the contexts are read, the layouts of contexts
    /// that no longer exist are dropped. When the file can't be read, the
    /// layouts stay, and new contexts take ids after theirs. The windows of
    /// the running apps then rejoin their contexts.
    fn read_contexts(&mut self, now: SystemTime) {
        self.contexts_unread = false;
        match self.contexts_store.load(now) {
            Loaded::Read { mut contexts, boot_id: saved } => {
                if saved.is_none() || saved != self.boot_id {
                    info!("The contexts were saved in another boot; forgetting window server ids");
                    contexts.forget_window_server_ids();
                }
                self.layout.retain_context_layouts(|id| contexts.get(id).is_some());
                self.contexts = contexts;
            }
            Loaded::Missing => {
                self.layout.retain_context_layouts(|_| false);
                self.contexts = Contexts::new();
            }
            Loaded::Unreadable => {
                self.contexts = empty_contexts_after(self.layout.context_ids().max());
            }
        }
        self.rejoin_every_window();
        if self.startup_complete {
            self.reconcile_cold_scope();
        }
    }

    /// A cold read happens before the screen and focused window are known.
    /// Once they are known, use that screen's saved context as the global one
    /// and clear the per-screen map before another command can save it.
    pub(super) fn reconcile_cold_scope(&mut self) -> bool {
        if self.scope() != Scope::Global
            || !self.contexts.has_screen_actives()
            || self.screens.is_empty()
        {
            return false;
        }
        let screen = self.screens[self.current_main_screen_index().unwrap_or(0)].id;
        let key = self.contexts.active_on(screen);
        info!(?screen, ?key, "Restoring the focused screen's context as global");
        self.contexts.forget_screen_actives();
        if let Err(err) = self.contexts.switch_to(key) {
            error!(?key, "Could not restore the focused screen's context: {err}");
        }
        self.save_contexts();
        true
    }

    /// Reads the contexts when a config reload turns contexts on, and records
    /// them: a replay has no `contexts.json` to read, so it applies the
    /// contexts from the recording.
    fn read_contexts_after_reload(&mut self, now: SystemTime, previous_scope: Option<Scope>) {
        self.read_contexts(now);
        if let Some(scope) = previous_scope {
            self.scope_changed(scope);
        }
        let event = Event::ContextsRead(Box::new(self.contexts.clone()));
        self.record.on_event(&event);
    }

    /// Sets the contexts that a recorded reload read, and applies them as the
    /// reload did.
    pub(super) fn contexts_read(&mut self, contexts: Contexts) {
        self.contexts = contexts;
        self.contexts_unread = false;
        self.layout.retain_context_layouts(|id| self.contexts.get(id).is_some());
        self.rejoin_every_window();
        if self.startup_complete {
            self.reconcile_cold_scope();
        }
        if self.contexts_in_use() {
            self.apply_again_focusing_parked_main();
        }
    }

    /// Has the windows of the running apps rejoin the contexts whose records
    /// they match, as at launch. Windows found while contexts were off
    /// have no membership until then.
    fn rejoin_every_window(&mut self) {
        let mut wids: Vec<WindowId> = self
            .windows
            .keys()
            .copied()
            .filter(|wid| self.apps.contains_key(&wid.pid))
            .collect();
        wids.sort();
        if self.rejoin_windows(&wids) {
            self.save_contexts();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use tempfile::TempDir;
    use test_log::test;
    use tokio::sync::mpsc;

    use super::super::testing::*;
    use super::super::{
        Command, ContextCommand, ContextRef, Event, Reactor, ReactorCommand, Requested,
    };
    use crate::actor::app::{Quiet, Request, WindowId};
    use crate::actor::contexts_store::{ContextsStore, Loaded};
    use crate::actor::layout::{LayoutCommand, LayoutEvent, LayoutManager};
    use crate::actor::parked_journal::{FailingWrites, JournalEntry, ParkedJournal};
    use crate::actor::raise;
    use crate::config::Config;
    use crate::model::Direction;
    use crate::model::contexts::{ContextId, ContextKey, RecordLink, Scope, WindowDesc};
    use crate::sys::app::WindowInfo;
    use crate::sys::event::MouseState;
    use crate::sys::screen::{CoordinateConverter, ScreenId, SpaceId};
    use crate::sys::window_server::{WindowServerId, WindowServerInfo, WindowsOnScreen};

    mod focus;
    mod focus_rules;
    mod management;
    mod membership;
    mod membership_rules;
    mod native_tabs;
    mod replay_rules;
    mod scope;
    mod switcher;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    fn wid(idx: u32) -> WindowId {
        WindowId::new(1, idx)
    }

    fn space() -> SpaceId {
        SpaceId::new(1)
    }

    /// A screen on which up to four windows side by side get tiles with
    /// whole-number frames.
    fn screen() -> CGRect {
        rect(0., 0., 1200., 1000.)
    }

    /// The corner that parks a window of `size` on `screen()`.
    fn corner(size: CGSize) -> CGRect {
        CGRect {
            origin: CGPoint::new(1199., 999.),
            size,
        }
    }

    /// The config of a test reactor, with contexts on or off.
    fn config(contexts: bool) -> Arc<Config> {
        let mut config = Config::default();
        config.settings.default_disable = false;
        config.settings.animate = false;
        config.settings.experimental.contexts.enable = contexts;
        Arc::new(config)
    }

    fn screens(frames: Vec<CGRect>, spaces: Vec<Option<SpaceId>>) -> Event {
        let ids = (1..=frames.len() as u32).map(ScreenId::new).collect();
        Event::ScreenParametersChanged {
            ids,
            bounds: frames.clone(),
            scale_factors: vec![1.0; frames.len()],
            frames,
            spaces,
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        }
    }

    fn frame_writes(requests: &[Request], wid: WindowId) -> Vec<CGRect> {
        requests
            .iter()
            .filter_map(|request| match request {
                Request::SetWindowFrame(request_wid, frame, _) if *request_wid == wid => {
                    Some(*frame)
                }
                _ => None,
            })
            .collect()
    }

    /// A reactor with contexts on, its journal and `contexts.json` in a
    /// temporary directory.
    struct Setup {
        reactor: Reactor,
        apps: Apps,
        dir: TempDir,
    }

    impl Setup {
        /// App 1's `windows` windows tiled side by side on one screen.
        fn new(windows: usize) -> Setup {
            let mut s = Setup::on(vec![screen()], vec![Some(space())]);
            s.reactor.handle_events(s.apps.make_app(1, make_windows(windows)));
            s.reactor.handle_event(Event::StartupComplete);
            s.apps.simulate_until_quiet(&mut s.reactor);
            s
        }

        /// A reactor with contexts on that no app has reached yet.
        fn on(frames: Vec<CGRect>, spaces: Vec<Option<SpaceId>>) -> Setup {
            let dir = TempDir::new().unwrap();
            let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
            reactor.journal =
                ParkedJournal::open(dir.path().join("parked.json"), SystemTime::now());
            reactor.open_contexts(
                ContextsStore::new(dir.path().join("contexts.json")),
                Some("boot".into()),
                SystemTime::now(),
            );
            reactor.handle_event(Event::ConfigChanged(config(true)));
            reactor.handle_event(screens(frames, spaces));
            Setup {
                reactor,
                apps: Apps::new(),
                dir,
            }
        }

        fn desc(&self, wid: WindowId) -> WindowDesc {
            let window = &self.reactor.windows[&wid];
            WindowDesc {
                wid,
                bundle_id: Some(format!("com.testapp{}", wid.pid)),
                app_name: Some(format!("TestApp{}", wid.pid)),
                title: window.title.expose_secret().clone(),
                window_server_id: window.window_server_id,
            }
        }

        /// Creates a context that holds `members`.
        fn create(&mut self, name: &str, members: &[WindowId]) -> ContextKey {
            let id = self.reactor.contexts.create(name).unwrap();
            for &wid in members {
                self.add(ContextKey::Named(id), wid);
            }
            ContextKey::Named(id)
        }

        fn add(&mut self, key: ContextKey, wid: WindowId) {
            let ContextKey::Named(id) = key else { panic!("{key:?}") };
            let desc = self.desc(wid);
            self.reactor.contexts.add_window(id, &desc).unwrap();
        }

        /// Sends the command that switches to `key`, as a key binding does.
        fn command(&mut self, key: ContextKey) {
            let command = match key {
                ContextKey::Everything => ContextCommand::ShowEverything,
                ContextKey::Unsorted => {
                    ContextCommand::SwitchContext(ContextRef::Name("Unsorted".into()))
                }
                ContextKey::Named(id) => ContextCommand::SwitchContext(ContextRef::Id(id)),
            };
            self.reactor.handle_event(Event::Command(Command::Context(command)));
        }

        /// Switches to `key`, and lets the apps answer.
        fn switch(&mut self, key: ContextKey) {
            self.command(key);
            self.apps.simulate_until_quiet(&mut self.reactor);
        }

        fn frame(&self, wid: WindowId) -> CGRect {
            self.apps.windows[&wid].frame
        }

        fn frames(&self, wids: &[WindowId]) -> Vec<(WindowId, CGRect)> {
            wids.iter().map(|&wid| (wid, self.frame(wid))).collect()
        }

        fn tiles_on(&self, space: SpaceId, screen: CGRect) -> Vec<(WindowId, CGRect)> {
            let mut tiles =
                self.reactor.layout.calculate_layout(space, screen, &self.reactor.config);
            tiles.sort_by_key(|(wid, _)| *wid);
            tiles
        }

        fn tiles(&self) -> Vec<(WindowId, CGRect)> {
            self.tiles_on(space(), screen())
        }

        fn parked(&self) -> Vec<WindowId> {
            let mut parked: Vec<WindowId> = self.reactor.parked.keys().copied().collect();
            parked.sort();
            parked
        }

        fn journal_on_disk(&self) -> Vec<JournalEntry> {
            ParkedJournal::open(self.dir.path().join("parked.json"), SystemTime::now())
                .entries()
                .to_vec()
        }

        /// Config with contexts on, in `scope`.
        fn scope(&mut self, scope: Scope) {
            let mut config = Config::default();
            config.settings.default_disable = false;
            config.settings.animate = false;
            config.settings.experimental.contexts.enable = true;
            config.settings.experimental.contexts.scope = scope;
            self.reactor.handle_event(Event::ConfigChanged(Arc::new(config)));
            self.apps.simulate_until_quiet(&mut self.reactor);
        }

        /// The context the screen with display id `screen` shows.
        fn active_on(&self, screen: u32) -> ContextKey {
            self.reactor.contexts.active_on(ScreenId::new(screen))
        }

        /// The context the screen with display id `screen` shows, as saved.
        fn saved_active_on(&self, screen: u32) -> ContextKey {
            match ContextsStore::new(self.dir.path().join("contexts.json")).load(SystemTime::now())
            {
                Loaded::Read { contexts, .. } => contexts.active_on(ScreenId::new(screen)),
                other => panic!("{other:?}"),
            }
        }

        /// Makes `screen` the focused one, as a main window there does.
        fn focus_screen(&mut self, screen: usize) {
            self.reactor.active_screen_idx = Some(screen as u16);
        }

        fn saved_active(&self) -> ContextKey {
            match ContextsStore::new(self.dir.path().join("contexts.json")).load(SystemTime::now())
            {
                Loaded::Read { contexts, .. } => contexts.active(),
                other => panic!("{other:?}"),
            }
        }

        /// Focuses the window and moves it, as the user does with a key.
        fn move_window(&mut self, wid: WindowId, direction: Direction) {
            self.reactor.send_layout_event(LayoutEvent::WindowFocused(vec![space()], wid));
            self.reactor
                .handle_event(Event::Command(Command::Layout(LayoutCommand::MoveNode(
                    direction,
                ))));
            self.apps.simulate_until_quiet(&mut self.reactor);
        }

        /// Closes the window. Its app forgets it, and the reactor learns that
        /// it was destroyed.
        fn close(&mut self, wid: WindowId) {
            self.apps.windows.remove(&wid);
            self.reactor.handle_event(Event::WindowDestroyed(wid));
        }
    }

    /// A window server snapshot that lists the windows at their frames.
    fn on_screen(s: &Setup, wids: &[WindowId]) -> WindowsOnScreen {
        WindowsOnScreen::new(
            wids.iter()
                .map(|&wid| WindowServerInfo {
                    id: s.reactor.windows[&wid].window_server_id.unwrap(),
                    pid: wid.pid,
                    layer: 0,
                    frame: s.frame(wid),
                })
                .collect(),
        )
    }

    fn entry(idx: u32, frame: CGRect) -> JournalEntry {
        JournalEntry {
            pid: 1,
            bundle_id: Some("com.testapp1".into()),
            window_server_id: WindowServerId::new(idx),
            title: format!("Window{idx}"),
            frame: frame.into(),
        }
    }

    /// R12, L1. The regression test for keeping layouts across switches.
    #[test]
    fn r12_switching_away_from_a_context_and_back_gives_the_same_frames() {
        let mut s = Setup::new(4);
        let all = [wid(1), wid(2), wid(3), wid(4)];
        let everything = vec![
            (wid(1), rect(0., 0., 300., 1000.)),
            (wid(2), rect(300., 0., 300., 1000.)),
            (wid(3), rect(600., 0., 300., 1000.)),
            (wid(4), rect(900., 0., 300., 1000.)),
        ];
        assert_eq!(everything, s.frames(&all));
        let c = s.create("C", &[wid(1), wid(2), wid(3)]);
        let d = s.create("D", &[wid(2), wid(3), wid(4)]);

        s.switch(c);
        s.move_window(wid(1), Direction::Right);
        let in_c = vec![
            (wid(1), rect(400., 0., 400., 1000.)),
            (wid(2), rect(0., 0., 400., 1000.)),
            (wid(3), rect(800., 0., 400., 1000.)),
        ];
        assert_eq!(in_c, s.frames(&[wid(1), wid(2), wid(3)]));
        assert_eq!(in_c, s.tiles());
        assert_eq!(corner(CGSize::new(300., 1000.)), s.frame(wid(4)));
        // The parked window reaches the layout again and gets no tile in C.
        s.reactor.update_visible_windows();
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(in_c, s.tiles());

        s.switch(d);
        s.move_window(wid(4), Direction::Left);
        let in_d = vec![
            (wid(2), rect(0., 0., 400., 1000.)),
            (wid(3), rect(800., 0., 400., 1000.)),
            (wid(4), rect(400., 0., 400., 1000.)),
        ];
        assert_eq!(in_d, s.frames(&[wid(2), wid(3), wid(4)]));
        assert_eq!(corner(CGSize::new(400., 1000.)), s.frame(wid(1)));

        for _ in 0..2 {
            s.switch(c);
            assert_eq!(in_c, s.frames(&[wid(1), wid(2), wid(3)]));
            assert_eq!(vec![wid(4)], s.parked());
            s.switch(d);
            assert_eq!(in_d, s.frames(&[wid(2), wid(3), wid(4)]));
            assert_eq!(vec![wid(1)], s.parked());
        }
        s.switch(ContextKey::Everything);
        assert_eq!(everything, s.frames(&all));
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
    }

    /// R13, R14. The window of Sugarglider itself, a panel the layout leaves
    /// alone, a minimized window, and a window on a screen whose Space is
    /// off all stay.
    #[test]
    fn r13_r14_a_switch_parks_every_non_member_on_the_visible_spaces_and_nothing_else() {
        let right = rect(1200., 0., 1200., 1000.);
        let mut s = Setup::on(vec![screen(), right], vec![Some(space()), None]);
        let own_pid = std::process::id() as i32;
        let own = WindowId::new(own_pid, 1);
        let own_window = WindowInfo {
            sys_id: Some(WindowServerId::new(50)),
            ..make_window(1)
        };
        s.reactor.handle_events(s.apps.make_app(own_pid, vec![own_window]));
        let on_right = WindowInfo {
            frame: rect(1300., 100., 50., 50.),
            ..make_window(5)
        };
        let mut windows = make_windows(4);
        windows.push(on_right);
        let mut launch = s.apps.make_app(1, windows);
        // Window 4 is a panel on a layer of its own.
        for event in &mut launch {
            if let Event::WindowsOnScreenUpdated { on_screen, .. } = event {
                on_screen.info[3].layer = 1;
            }
        }
        s.reactor.handle_events(launch);
        s.reactor.handle_event(Event::StartupComplete);
        // Window 3 is minimized.
        let visible = |id: u32, layer: i32, frame: CGRect| WindowServerInfo {
            id: WindowServerId::new(id),
            pid: if id == 50 { own_pid } else { 1 },
            layer,
            frame,
        };
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(vec![
                visible(50, 0, rect(100., 100., 50., 50.)),
                visible(1, 0, rect(100., 100., 50., 50.)),
                visible(2, 0, rect(200., 100., 50., 50.)),
                visible(4, 1, rect(400., 100., 50., 50.)),
                visible(5, 0, rect(1300., 100., 50., 50.)),
            ]),
        });
        s.reactor.update_visible_windows();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let tiles = vec![
            (own, rect(0., 0., 400., 1000.)),
            (wid(1), rect(400., 0., 400., 1000.)),
            (wid(2), rect(800., 0., 400., 1000.)),
        ];
        let mut before = s.tiles();
        before.sort_by_key(|(wid, _)| *wid != own);
        assert_eq!(tiles, before);
        let untouched = [own, wid(3), wid(4), wid(5)].map(|wid| (wid, s.frame(wid)));
        let c = s.create("C", &[wid(1)]);

        s.switch(c);

        assert_eq!(vec![wid(2)], s.parked());
        // The bottom right corner would reach into the screen on the right.
        assert_eq!(rect(-399., 999., 400., 1000.), s.frame(wid(2)));
        assert_eq!(
            untouched,
            [own, wid(3), wid(4), wid(5)].map(|wid| (wid, s.frame(wid)))
        );
        assert_eq!(vec![entry(2, rect(800., 0., 400., 1000.))], s.journal_on_disk());
    }

    /// L8. The window joins C while D is active, so C's layout has no node
    /// for it.
    #[test]
    fn l8_a_member_without_a_node_in_the_targets_layout_is_put_back_and_tiled() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1)]);
        let d = s.create("D", &[wid(2), wid(3)]);
        s.switch(c);
        s.switch(d);
        assert_eq!(vec![wid(1)], s.parked());
        s.add(c, wid(3));

        s.switch(c);

        let tiles = vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(3), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(tiles, s.tiles());
        assert_eq!(tiles, s.frames(&[wid(1), wid(3)]));
        assert_eq!(vec![wid(2)], s.parked());
        assert_eq!(corner(CGSize::new(600., 1000.)), s.frame(wid(2)));
        assert_eq!(vec![entry(2, rect(0., 0., 600., 1000.))], s.journal_on_disk());
    }

    /// Makes window 1 float at the frame it had before it was tiled,
    /// `(100, 100, 50, 50)`.
    fn float_window_1(s: &mut Setup) {
        s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
        s.reactor.send_layout_event(LayoutEvent::WindowFocused(vec![space()], wid(1)));
        s.reactor.handle_event(Event::Command(Command::Layout(
            LayoutCommand::ToggleWindowFloating,
        )));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(rect(100., 100., 50., 50.), s.frame(wid(1)));
    }

    /// H3.
    #[test]
    fn h3_a_floating_member_comes_back_at_its_journal_frame() {
        let mut s = Setup::new(2);
        float_window_1(&mut s);
        let floating = rect(100., 100., 50., 50.);
        let c = s.create("C", &[wid(2)]);
        let d = s.create("D", &[wid(1), wid(2)]);
        s.switch(c);
        assert_eq!(corner(floating.size), s.frame(wid(1)));
        assert_eq!(vec![entry(1, floating)], s.journal_on_disk());

        s.command(d);

        let requests = s.apps.requests();
        assert_eq!(vec![floating], frame_writes(&requests, wid(1)));
        for event in s.apps.simulate_events_for_requests(requests) {
            s.reactor.handle_event(event);
        }
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(floating, s.frame(wid(1)));
        assert_eq!(vec![(wid(2), screen())], s.tiles());
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
    }

    /// H4.
    #[test]
    fn h4_toggling_between_two_contexts_ten_times_quickly_ends_at_the_right_frames() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1), wid(2)]);
        let d = s.create("D", &[wid(2), wid(3)]);
        s.switch(c);
        s.switch(d);
        let in_d = vec![
            (wid(2), rect(0., 0., 600., 1000.)),
            (wid(3), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(in_d, s.frames(&[wid(2), wid(3)]));

        for _ in 0..10 {
            s.command(c);
            s.command(d);
        }
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(in_d, s.frames(&[wid(2), wid(3)]));
        assert_eq!(corner(CGSize::new(600., 1000.)), s.frame(wid(1)));
        assert_eq!(vec![wid(1)], s.parked());
        assert_eq!(vec![entry(1, rect(0., 0., 600., 1000.))], s.journal_on_disk());
        s.switch(c);
        assert_eq!(
            vec![
                (wid(1), rect(0., 0., 600., 1000.)),
                (wid(2), rect(600., 0., 600., 1000.)),
            ],
            s.frames(&[wid(1), wid(2)])
        );
        assert_eq!(vec![wid(3)], s.parked());
    }

    /// H5.
    #[test]
    fn h5_a_parked_window_that_closes_during_a_switch_cycle_leaves_no_empty_tile() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1)]);
        let d = s.create("D", &[wid(2), wid(3)]);
        s.switch(d);
        s.switch(c);
        assert_eq!(vec![wid(2), wid(3)], s.parked());
        assert_eq!(s.frame(wid(2)), s.frame(wid(3)), "both share one corner");

        s.close(wid(2));
        s.switch(d);

        assert_eq!(vec![(wid(3), screen())], s.tiles());
        assert_eq!(screen(), s.frame(wid(3)));
        assert_eq!(vec![wid(1)], s.parked());
        assert_eq!(vec![entry(1, screen())], s.journal_on_disk());
    }

    /// R12, R30.
    #[test]
    fn r12_r30_a_failed_journal_write_stops_the_switch_and_keeps_the_old_context() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1)]);
        let frames = s.frames(&[wid(1), wid(2)]);
        let txids = [wid(1), wid(2)].map(|wid| s.reactor.windows[&wid].last_sent_txid);

        let failing = FailingWrites::start(s.dir.path());
        s.command(c);
        drop(failing);

        assert!(s.apps.requests().is_empty());
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert_eq!(None, s.reactor.contexts.previous());
        assert!(s.parked().is_empty());
        assert_eq!(frames, s.tiles());
        assert_eq!(
            txids,
            [wid(1), wid(2)].map(|wid| s.reactor.windows[&wid].last_sent_txid)
        );
        assert_eq!(0, s.reactor.layout.context_ids().count());
        assert!(!s.dir.path().join("contexts.json").exists());
        assert!(s.journal_on_disk().is_empty());

        s.switch(c);
        assert_eq!(vec![wid(2)], s.parked());
        assert_eq!(c, s.saved_active());
    }

    /// R27.
    #[test]
    fn r27_showing_everything_puts_every_window_back() {
        let mut s = Setup::new(3);
        float_window_1(&mut s);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(2)]);
        let d = s.create("D", &[wid(3)]);
        s.switch(c);
        s.switch(d);
        assert_eq!(vec![wid(1), wid(2)], s.parked());

        s.switch(ContextKey::Everything);

        assert_eq!(everything, s.frames(&all));
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
        assert_eq!(ContextKey::Everything, s.saved_active());
    }

    /// L10, R7.
    #[test]
    fn l10_r7_a_window_moved_to_another_screen_under_c_stays_there_under_d() {
        let left = screen();
        let right = rect(1200., 0., 1200., 1000.);
        let mut s = Setup::on(vec![left, right], vec![Some(space()), Some(SpaceId::new(2))]);
        let on_right = WindowInfo {
            frame: rect(1300., 100., 50., 50.),
            ..make_window(3)
        };
        s.reactor
            .handle_events(s.apps.make_app(1, vec![make_window(1), make_window(2), on_right]));
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        let c = s.create("C", &[wid(1), wid(2), wid(3)]);
        let d = s.create("D", &[wid(1), wid(2), wid(3)]);
        s.switch(d);
        s.switch(c);
        assert_eq!(
            vec![
                (wid(1), rect(0., 0., 600., 1000.)),
                (wid(2), rect(600., 0., 600., 1000.)),
            ],
            s.tiles_on(space(), left)
        );

        // The user drags window 2 onto the right screen.
        let dropped = rect(1400., 100., 600., 1000.);
        let txid = s.reactor.windows[&wid(2)].last_sent_txid;
        s.apps.windows.get_mut(&wid(2)).unwrap().frame = dropped;
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(2),
            dropped,
            txid,
            Requested(false),
            Some(MouseState::Up),
        ));
        s.apps.simulate_until_quiet(&mut s.reactor);
        let right_tiles = vec![
            (wid(2), rect(1800., 0., 600., 1000.)),
            (wid(3), rect(1200., 0., 600., 1000.)),
        ];
        assert_eq!(vec![(wid(1), left)], s.tiles_on(space(), left));
        assert_eq!(right_tiles, s.tiles_on(SpaceId::new(2), right));

        s.switch(d);

        // D's layout on the right screen takes window 2 in the order of the
        // windows' first frames.
        let right_tiles = vec![
            (wid(2), rect(1200., 0., 600., 1000.)),
            (wid(3), rect(1800., 0., 600., 1000.)),
        ];
        assert_eq!(vec![(wid(1), left)], s.tiles_on(space(), left));
        assert_eq!(right_tiles, s.tiles_on(SpaceId::new(2), right));
        assert_eq!(right_tiles, s.frames(&[wid(2), wid(3)]));
        assert_eq!(left, s.frame(wid(1)));
        assert!(s.parked().is_empty());
    }

    /// R10, L2.
    #[test]
    fn r10_l2_a_space_change_makes_the_contexts_layout_and_applies_it_in_the_same_event() {
        let mut s = Setup::new(3);
        // Window 3 is on another Space.
        let snapshot = on_screen(&s, &[wid(1), wid(2)]);
        s.reactor
            .handle_event(Event::WindowsOnScreenUpdated { pid: None, on_screen: snapshot });
        s.reactor.update_visible_windows();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        assert_eq!(vec![(wid(1), screen())], s.tiles());
        assert_eq!(vec![wid(2)], s.parked());

        // The user moves to Space 2, which shows windows 1 and 3.
        let space2 = SpaceId::new(2);
        let snapshot = on_screen(&s, &[wid(1), wid(3)]);
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space2)], snapshot));

        assert_eq!(vec![(wid(1), screen())], s.tiles_on(space2, screen()));
        assert_eq!(vec![wid(2), wid(3)], s.parked());
        let requests = s.apps.requests();
        assert_eq!(
            vec![corner(CGSize::new(400., 1000.))],
            frame_writes(&requests, wid(3))
        );
        assert!(frame_writes(&requests, wid(1)).is_empty());
        assert!(s.reactor.layout.context_ids().all(|id| ContextKey::Named(id) == c));
    }

    /// R16. A journal failure while window 3 returns ends the context.
    /// A later switch parks both nonmembers after the journal recovers.
    #[test]
    fn r16_switching_to_the_active_context_parks_windows_that_drifted_in() {
        let mut s = Setup::new(3);
        let unminimized = s.frame(wid(3));
        report_visible(&mut s, &[wid(1), wid(2)]);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        assert_eq!(vec![wid(2)], s.parked());
        let failing = FailingWrites::start(s.dir.path());
        report_visible(&mut s, &[wid(1), wid(2), wid(3)]);
        drop(failing);
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert!(s.parked().is_empty());

        s.switch(c);

        assert_eq!(vec![wid(2), wid(3)], s.parked());
        assert_eq!(corner(unminimized.size), s.frame(wid(3)));
        assert_eq!(vec![(wid(1), screen())], s.tiles());
    }

    /// R18, R19.
    #[test]
    fn r18_previous_context_goes_back_to_the_context_used_before() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1)]);
        let d = s.create("D", &[wid(2)]);
        let previous = |s: &mut Setup| {
            s.reactor
                .handle_event(Event::Command(Command::Context(ContextCommand::PreviousContext)));
            s.apps.simulate_until_quiet(&mut s.reactor);
        };
        previous(&mut s);
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());

        s.switch(c);
        s.switch(d);
        let used = |s: &Setup, key| s.reactor.contexts.last_used(key);
        assert!(used(&s, d) > used(&s, c) && used(&s, c) > 0, "R19");
        previous(&mut s);
        assert_eq!(c, s.reactor.contexts.active());
        assert_eq!(vec![wid(2)], s.parked());
        previous(&mut s);
        assert_eq!(d, s.reactor.contexts.active());
        assert_eq!(vec![wid(1)], s.parked());
        s.switch(d);
        previous(&mut s);
        assert_eq!(c, s.reactor.contexts.active());
    }

    /// R29, R3. A pinned window shows under every context.
    #[test]
    fn r29_unsorted_shows_the_windows_in_no_context_and_the_pinned_ones() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1)]);
        let desc = s.desc(wid(3));
        s.reactor.contexts.pin(&desc);

        s.switch(ContextKey::Unsorted);
        assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
        assert_eq!(vec![wid(1)], s.parked());
        assert_eq!(
            vec![
                (wid(2), rect(0., 0., 600., 1000.)),
                (wid(3), rect(600., 0., 600., 1000.)),
            ],
            s.tiles()
        );

        s.switch(c);
        assert_eq!(vec![wid(2)], s.parked());
        assert_eq!(
            vec![
                (wid(1), rect(0., 0., 600., 1000.)),
                (wid(3), rect(600., 0., 600., 1000.)),
            ],
            s.tiles()
        );
    }

    /// R12, step 5.
    #[test]
    fn r12_a_switch_focuses_the_targets_most_recently_focused_window() {
        let mut s = Setup::new(3);
        let focus = |s: &mut Setup, wid: WindowId| {
            s.reactor.handle_event(Event::ApplicationActivated(1, Quiet::No));
            s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
            s.reactor
                .handle_event(Event::ApplicationMainWindowChanged(1, Some(wid), Quiet::No));
        };
        focus(&mut s, wid(2));
        focus(&mut s, wid(1));
        focus(&mut s, wid(3));
        let c = s.create("C", &[wid(1), wid(2)]);
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        s.reactor.raise_manager_tx = raise_manager_tx;

        s.switch(c);

        let mut focused = vec![];
        while let Ok((_, event)) = raise_manager_rx.try_recv() {
            if let raise::Event::RaiseRequest(request) = event {
                focused.extend(request.focus_window.map(|(wid, _)| wid));
            }
        }
        assert_eq!(vec![wid(1)], focused);
    }

    #[test]
    fn a_switch_during_a_drag_ends_the_drag_first() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1)]);
        s.reactor.handle_event(Event::LeftMouseDown(
            CGPoint::new(300., 500.),
            Some(WindowServerId::new(1)),
        ));
        assert!(s.reactor.in_drag);

        s.switch(c);

        assert!(!s.reactor.in_drag);
        assert!(!s.reactor.layout.has_interactive_state());
        assert!(s.reactor.title_bar_drag.is_none());
        assert_eq!(vec![(wid(1), screen())], s.tiles());
        s.reactor.handle_event(Event::LeftMouseDragged(CGPoint::new(900., 500.)));
        s.reactor.handle_event(Event::MouseUp);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![(wid(1), screen())], s.tiles());
        assert_eq!(screen(), s.frame(wid(1)));
    }

    #[test]
    fn a_switch_to_a_context_that_doesnt_exist_changes_nothing() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1)]);
        let ContextKey::Named(id) = c else { unreachable!() };
        s.reactor.contexts.delete(id).unwrap();

        s.command(c);
        s.reactor
            .handle_event(Event::Command(Command::Context(ContextCommand::SwitchContext(
                ContextRef::Number(1),
            ))));

        assert!(s.apps.requests().is_empty());
        assert!(s.parked().is_empty());
        assert_eq!(0, s.reactor.layout.context_ids().count());
    }

    #[test]
    fn a_command_names_a_context_by_number_name_or_id() {
        let mut s = Setup::new(3);
        let c = s.create("Client work", &[wid(1)]);
        let d = s.create("Comms", &[wid(2)]);
        let run = |s: &mut Setup, reference: ContextRef| {
            s.reactor.handle_event(Event::Command(Command::Context(
                ContextCommand::SwitchContext(reference),
            )));
            s.apps.simulate_until_quiet(&mut s.reactor);
            s.reactor.contexts.active()
        };
        assert_eq!(d, run(&mut s, ContextRef::Number(2)));
        assert_eq!(c, run(&mut s, ContextRef::Name("cli".into())));
        assert_eq!(d, run(&mut s, ContextRef::Name("COMMS".into())));
        assert_eq!(
            ContextKey::Unsorted,
            run(&mut s, ContextRef::Name("unsorted".into()))
        );
        let ContextKey::Named(id) = c else { unreachable!() };
        assert_eq!(c, run(&mut s, ContextRef::Id(id)));
        assert_eq!(c, run(&mut s, ContextRef::Name("nothing like it".into())));
    }

    /// R28.
    #[test]
    fn r28_with_contexts_off_nothing_changes() {
        let dir = TempDir::new().unwrap();
        // Contexts from a session with contexts on, with C active.
        let mut saved = crate::model::contexts::Contexts::new();
        let id = saved.create("C").unwrap();
        saved.switch_to(ContextKey::Named(id)).unwrap();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.journal = ParkedJournal::open(dir.path().join("parked.json"), SystemTime::now());
        reactor.open_contexts(
            ContextsStore::new(dir.path().join("contexts.json")),
            Some("boot".into()),
            SystemTime::now(),
        );
        reactor.contexts = saved;
        let mut apps = Apps::new();
        reactor.handle_event(screens(vec![screen()], vec![Some(space())]));
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);
        let desc = WindowDesc {
            wid: wid(1),
            bundle_id: Some("com.testapp1".into()),
            app_name: Some("TestApp1".into()),
            title: "Window1".into(),
            window_server_id: Some(WindowServerId::new(1)),
        };
        reactor.contexts.add_window(id, &desc).unwrap();
        let tiles = vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(2), rect(600., 0., 600., 1000.)),
        ];

        for command in [
            ContextCommand::SwitchContext(ContextRef::Id(id)),
            ContextCommand::SwitchContext(ContextRef::Number(1)),
            ContextCommand::ShowEverything,
            ContextCommand::PreviousContext,
        ] {
            reactor.handle_event(Event::Command(Command::Context(command)));
            assert!(apps.requests().is_empty());
        }
        reactor.handle_event(Event::SpaceChanged(
            vec![Some(SpaceId::new(2))],
            Default::default(),
        ));
        apps.simulate_until_quiet(&mut reactor);
        let shorter = rect(0., 0., 1200., 900.);
        reactor.handle_event(screens(vec![shorter], vec![Some(space())]));
        apps.simulate_until_quiet(&mut reactor);
        reactor.handle_event(screens(vec![screen()], vec![Some(space())]));
        apps.simulate_until_quiet(&mut reactor);

        let mut after = reactor.layout.calculate_layout(space(), screen(), &reactor.config);
        after.sort_by_key(|(wid, _)| *wid);
        assert_eq!(tiles, after);
        assert_eq!(
            tiles,
            [wid(1), wid(2)].map(|wid| (wid, apps.windows[&wid].frame))
        );
        assert!(reactor.parked.is_empty());
        assert!(reactor.layout.serialize_to_string().contains(",context_layouts:{},"));
        assert!(
            fs::read_dir(dir.path()).unwrap().next().is_none(),
            "no file is written"
        );

        let exits = Arc::new(Mutex::new(vec![]));
        let caught = exits.clone();
        reactor.layout_file = Some(dir.path().join("layout.ron"));
        reactor.exit = Box::new(move |code| caught.lock().unwrap().push(code));
        reactor.handle_event(Event::Command(Command::Reactor(ReactorCommand::SaveAndExit)));
        assert_eq!(vec![0], *exits.lock().unwrap());
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(vec!["layout.ron"], names);
    }

    /// R33, R28.
    #[test]
    fn turning_contexts_off_shows_every_window_and_turning_them_on_applies_the_context() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        assert_eq!(vec![wid(2), wid(3)], s.parked());

        s.reactor.handle_event(Event::ConfigChanged(config(false)));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(everything, s.frames(&all));
        assert_eq!(everything, s.tiles());
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
        assert_eq!(c, s.reactor.contexts.active());
        assert_eq!(c, s.saved_active());

        s.reactor.handle_event(Event::ConfigChanged(config(true)));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(vec![wid(2), wid(3)], s.parked());
        assert_eq!(vec![(wid(1), screen())], s.tiles());
        assert_eq!(screen(), s.frame(wid(1)));
    }

    #[test]
    fn contexts_json_is_written_after_each_switch_and_when_an_app_with_members_quits() {
        let mut s = Setup::new(2);
        let path = s.dir.path().join("contexts.json");
        assert!(!path.exists());
        let c = s.create("C", &[wid(1)]);

        s.switch(c);
        assert_eq!(c, s.saved_active());
        s.switch(ContextKey::Everything);
        assert_eq!(ContextKey::Everything, s.saved_active());

        fs::remove_file(&path).unwrap();
        s.reactor.handle_events(s.apps.make_app(2, vec![]));
        s.reactor.handle_event(Event::ApplicationThreadTerminated(2));
        assert!(!path.exists(), "app 2 has no member records");
        s.reactor.handle_event(Event::ApplicationThreadTerminated(1));
        assert!(path.exists());
        let written: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(serde_json::json!("boot"), written["boot_id"]);
        assert_eq!(
            serde_json::json!("Window1"),
            written["contexts"][0]["members"][0]["title"]
        );
    }

    /// Saves contexts with one member record, as a boot named `boot` did.
    /// The record has window 1's window server id and a title that window 1
    /// doesn't have now, so only the id can match window 1.
    fn saved_by_boot(s: &Setup, boot: Option<&str>) -> ContextsStore {
        let mut contexts = crate::model::contexts::Contexts::new();
        let id = contexts.create("C").unwrap();
        let desc = WindowDesc {
            title: "Before the restart".into(),
            ..s.desc(wid(1))
        };
        contexts.add_window(id, &desc).unwrap();
        let store = ContextsStore::new(s.dir.path().join("contexts.json"));
        store.save(&contexts, boot).unwrap();
        ContextsStore::new(s.dir.path().join("contexts.json"))
    }

    fn saved_window_server_id(s: &Setup) -> Option<WindowServerId> {
        s.reactor.contexts.contexts()[0].members[0].window_server_id
    }

    /// R22.
    #[test]
    fn r22_contexts_saved_in_another_boot_forget_their_window_server_ids() {
        let mut s = Setup::new(1);
        let now = SystemTime::now();

        let store = saved_by_boot(&s, Some("boot"));
        s.reactor.open_contexts(store, Some("boot".into()), now);
        assert_eq!(Some(WindowServerId::new(1)), saved_window_server_id(&s));

        let store = saved_by_boot(&s, Some("earlier boot"));
        s.reactor.open_contexts(store, Some("boot".into()), now);
        assert_eq!(None, saved_window_server_id(&s));

        let store = saved_by_boot(&s, None);
        s.reactor.open_contexts(store, Some("boot".into()), now);
        assert_eq!(None, saved_window_server_id(&s));

        let store = saved_by_boot(&s, Some("boot"));
        s.reactor.open_contexts(store, None, now);
        assert_eq!(None, saved_window_server_id(&s));
    }

    /// L2.
    #[test]
    fn l2_loading_drops_the_layouts_of_deleted_contexts_only_when_the_file_is_read() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1)]);
        let d = s.create("D", &[wid(2)]);
        s.switch(c);
        s.switch(d);
        s.switch(ContextKey::Everything);
        let (ContextKey::Named(c_id), ContextKey::Named(d_id)) = (c, d) else {
            unreachable!()
        };
        let ids = |s: &Setup| {
            let mut ids: Vec<_> = s.reactor.layout.context_ids().collect();
            ids.sort();
            ids.dedup();
            ids
        };
        assert_eq!(vec![c_id, d_id], ids(&s));
        let path = s.dir.path().join("contexts.json");
        let now = SystemTime::now();

        fs::write(&path, "{ not json").unwrap();
        s.reactor
            .open_contexts(ContextsStore::new(path.clone()), Some("boot".into()), now);
        assert_eq!(vec![c_id, d_id], ids(&s));
        assert!(s.reactor.contexts.contexts().is_empty());
        let new = s.reactor.contexts.create("New").unwrap();
        assert!(new > d_id, "{new:?}");

        let mut kept = crate::model::contexts::Contexts::new();
        assert_eq!(c_id, kept.create("C").unwrap());
        ContextsStore::new(path.clone()).save(&kept, Some("boot")).unwrap();
        s.reactor
            .open_contexts(ContextsStore::new(path.clone()), Some("boot".into()), now);
        assert_eq!(vec![c_id], ids(&s));

        fs::remove_file(&path).unwrap();
        s.reactor.open_contexts(ContextsStore::new(path), Some("boot".into()), now);
        assert!(ids(&s).is_empty());
    }

    /// Each form of `ContextRef` survives the RON round trip that recordings
    /// take. A bare integer is a number, and `Id(7)` is an id.
    #[test]
    fn every_context_ref_survives_a_ron_round_trip() {
        let id: ContextId = serde_json::from_value(serde_json::json!(7)).unwrap();
        let command =
            |reference| Event::Command(Command::Context(ContextCommand::SwitchContext(reference)));
        let read = |text: &str| match ron::de::from_str(text).unwrap() {
            Event::Command(Command::Context(ContextCommand::SwitchContext(reference))) => reference,
            other => panic!("{other:?}"),
        };
        for reference in [
            ContextRef::Number(7),
            ContextRef::Name("Comms".into()),
            ContextRef::Name("7".into()),
            ContextRef::Id(id),
        ] {
            let text = ron::ser::to_string(&command(reference.clone())).unwrap();
            assert_eq!(reference, read(&text), "{text}");
        }
        assert_eq!(ContextRef::Number(7), read("Command(switch_context(7))"));
        assert_eq!(ContextRef::Id(id), read("Command(switch_context(Id(7)))"));
        assert_eq!(ContextRef::Id(id), read("Command(switch_context(id(7)))"));
        assert!(ron::de::from_str::<Event>("Command(switch_context(Other(7)))").is_err());
        for command in [
            ContextCommand::ShowEverything,
            ContextCommand::PreviousContext,
        ] {
            let event = Event::Command(Command::Context(command.clone()));
            let text = ron::ser::to_string(&event).unwrap();
            let Event::Command(Command::Context(back)) = ron::de::from_str(&text).unwrap() else {
                panic!("{text}");
            };
            assert_eq!(command, back);
        }
    }

    /// Makes the reactor save its layout in the temporary directory, and
    /// returns the exit codes it quits with.
    fn catch_exits(s: &mut Setup) -> Arc<Mutex<Vec<i32>>> {
        let exits = Arc::new(Mutex::new(vec![]));
        let caught = exits.clone();
        s.reactor.layout_file = Some(s.dir.path().join("layout.ron"));
        s.reactor.exit = Box::new(move |code| caught.lock().unwrap().push(code));
        exits
    }

    fn save_and_exit(s: &mut Setup) {
        s.reactor
            .handle_event(Event::Command(Command::Reactor(ReactorCommand::SaveAndExit)));
    }

    /// R32, R34. A journal entry of an app that hasn't registered, left by an
    /// earlier run, doesn't hold up a quit, and it stays for the next launch.
    #[test]
    fn r32_a_journal_entry_of_an_app_that_hasnt_registered_doesnt_hold_up_a_quit() {
        let mut s = Setup::new(2);
        let leftover = journal_entry(7, 70, 1, rect(0., 0., 300., 300.));
        s.reactor.journal.record(vec![leftover.clone()]).unwrap();
        let exits = catch_exits(&mut s);

        save_and_exit(&mut s);

        assert_eq!(vec![0], *exits.lock().unwrap());
        assert_eq!(vec![leftover], s.journal_on_disk());
    }

    /// R32.
    #[test]
    fn r32_quitting_puts_every_parked_window_back_and_quits_after_the_last_is_back() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        let exits = catch_exits(&mut s);

        save_and_exit(&mut s);

        let requests = s.apps.requests();
        assert_eq!(vec![everything[1].1], frame_writes(&requests, wid(2)));
        assert_eq!(vec![everything[2].1], frame_writes(&requests, wid(3)));
        assert!(exits.lock().unwrap().is_empty());
        // Contexts commands wait for the quit.
        s.command(ContextKey::Everything);
        s.command(c);
        assert!(s.apps.requests().is_empty());
        assert!(s.parked().is_empty());

        let mut echoes = s.apps.simulate_events_for_requests(requests).into_iter();
        let first = echoes.next().unwrap();
        s.reactor.handle_event(first);
        assert!(exits.lock().unwrap().is_empty());
        for event in echoes {
            s.reactor.handle_event(event);
        }

        assert_eq!(vec![0], *exits.lock().unwrap());
        assert_eq!(everything, s.frames(&all));
        assert!(s.journal_on_disk().is_empty());
        assert_eq!(c, s.saved_active());
        let saved = fs::read_to_string(s.dir.path().join("layout.ron")).unwrap();
        assert!(saved.contains("context_layouts:{((1),Named(1))"), "{saved}");
        s.reactor.exit_deadline_tick(Instant::now() + Duration::from_secs(10));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![0], *exits.lock().unwrap(), "the quit happens once");
    }

    /// R32.
    #[test]
    fn r32_when_the_deadline_passes_first_the_journal_stays() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        let journal = s.journal_on_disk();
        assert_eq!(2, journal.len());
        let exits = catch_exits(&mut s);
        let start = Instant::now();

        save_and_exit(&mut s);
        save_and_exit(&mut s);
        s.reactor.exit_deadline_tick(start + Duration::from_millis(1900));
        assert!(exits.lock().unwrap().is_empty());
        s.reactor.exit_deadline_tick(Instant::now() + Duration::from_secs(2));

        assert_eq!(vec![0], *exits.lock().unwrap());
        assert_eq!(journal, s.journal_on_disk());
        assert_eq!(c, s.saved_active());
        assert!(s.dir.path().join("layout.ron").exists());
    }

    /// R32.
    #[test]
    fn r32_with_nothing_parked_quitting_is_at_once() {
        let mut s = Setup::new(2);
        let exits = catch_exits(&mut s);

        save_and_exit(&mut s);

        assert_eq!(vec![0], *exits.lock().unwrap());
        assert!(s.apps.requests().is_empty());
        assert_eq!(ContextKey::Everything, s.saved_active());
    }

    /// R32, R27. A quit that waits for windows a switch put back, with no
    /// window parked, shows Everything on the Space while it waits. A window
    /// that becomes visible meanwhile is tiled there, and the context keeps
    /// its arrangement.
    #[test]
    fn r32_a_quit_that_waits_with_nothing_parked_shows_everything() {
        let mut s = Setup::new(4);
        // Window 4 is minimized.
        let snapshot = on_screen(&s, &[wid(1), wid(2), wid(3)]);
        s.reactor
            .handle_event(Event::WindowsOnScreenUpdated { pid: None, on_screen: snapshot });
        s.reactor.update_visible_windows();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let c = s.create("C", &[wid(1)]);
        let d = s.create("D", &[wid(1), wid(2), wid(3)]);
        s.switch(d);
        s.move_window(wid(1), Direction::Right);
        let in_d = vec![
            (wid(1), rect(400., 0., 400., 1000.)),
            (wid(2), rect(0., 0., 400., 1000.)),
            (wid(3), rect(800., 0., 400., 1000.)),
        ];
        assert_eq!(in_d, s.tiles());
        s.command(c);
        s.command(d);
        let exits = catch_exits(&mut s);

        save_and_exit(&mut s);
        assert!(s.parked().is_empty());
        assert!(exits.lock().unwrap().is_empty());
        let mut requests = s.apps.requests();
        // Window 4 is unminimized, and the refresh reports it.
        let snapshot = on_screen(&s, &[wid(1), wid(2), wid(3), wid(4)]);
        s.reactor
            .handle_event(Event::WindowsOnScreenUpdated { pid: None, on_screen: snapshot });
        s.reactor.handle_event(Event::WindowsDiscovered {
            pid: 1,
            new: vec![],
            known_visible: vec![wid(1), wid(2), wid(3), wid(4)],
        });

        let everything = vec![
            (wid(1), rect(0., 0., 300., 1000.)),
            (wid(2), rect(300., 0., 300., 1000.)),
            (wid(3), rect(600., 0., 300., 1000.)),
            (wid(4), rect(900., 0., 300., 1000.)),
        ];
        assert_eq!(everything, s.tiles());
        requests.extend(s.apps.requests());
        answer(&mut s, requests);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![0], *exits.lock().unwrap());
        assert_eq!(everything, s.frames(&[wid(1), wid(2), wid(3), wid(4)]));
        assert!(s.journal_on_disk().is_empty());

        // The test reactor keeps running after the quit.
        s.switch(d);
        assert_eq!(in_d, s.tiles());
        assert_eq!(vec![wid(4)], s.parked());
    }

    /// R32, R34. With contexts off, a quit waits for a window that the
    /// journal put back at launch, until its app reports it back. If the
    /// deadline passes first, the journal keeps the window's entry.
    #[test]
    fn r32_with_contexts_off_a_quit_waits_for_a_window_the_journal_put_back() {
        let parked_at = corner(CGSize::new(600., 1000.));
        let before = rect(600., 0., 600., 1000.);
        for confirmed in [true, false] {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("parked.json");
            let mut journal = ParkedJournal::open(path.clone(), SystemTime::now());
            journal.record(vec![entry(2, before)]).unwrap();
            let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
            reactor.journal = ParkedJournal::open(path.clone(), SystemTime::now());
            reactor.handle_event(screens(vec![screen()], vec![Some(space())]));
            let mut apps = Apps::new();
            let windows = vec![
                make_window(1),
                WindowInfo {
                    frame: parked_at,
                    ..make_window(2)
                },
            ];
            reactor.handle_events(apps.make_app(1, windows));
            reactor.handle_event(Event::StartupComplete);
            let requests = apps.requests();
            assert_eq!(vec![before], frame_writes(&requests, wid(2)));
            let exits = Arc::new(Mutex::new(vec![]));
            let caught = exits.clone();
            reactor.exit = Box::new(move |code| caught.lock().unwrap().push(code));
            let start = Instant::now();

            reactor.handle_event(Event::Command(Command::Reactor(ReactorCommand::SaveAndExit)));
            assert!(exits.lock().unwrap().is_empty());
            assert!(apps.requests().is_empty());

            if confirmed {
                for event in apps.simulate_events_for_requests(requests) {
                    reactor.handle_event(event);
                }
                assert_eq!(vec![0], *exits.lock().unwrap());
                assert!(ParkedJournal::open(path, SystemTime::now()).entries().is_empty());
            } else {
                reactor.exit_deadline_tick(start + Duration::from_millis(1900));
                assert!(exits.lock().unwrap().is_empty());
                reactor.exit_deadline_tick(Instant::now() + Duration::from_secs(2));
                assert_eq!(vec![0], *exits.lock().unwrap());
                assert_eq!(
                    vec![entry(2, before)],
                    ParkedJournal::open(path, SystemTime::now()).entries()
                );
            }
        }
    }

    /// R33.
    #[test]
    fn r33_turning_off_shows_everything_first_and_turning_on_applies_the_context_again() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        assert_eq!(vec![wid(2), wid(3)], s.parked());

        s.reactor.handle_event(Event::ShowEverythingOn(vec![space()]));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(everything, s.frames(&all));
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
        assert_eq!(c, s.reactor.contexts.active());

        s.reactor.handle_event(Event::SpaceChanged(vec![None], Default::default()));
        let requests = s.apps.requests();
        assert!(all.iter().all(|&wid| frame_writes(&requests, wid).is_empty()));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(everything, s.frames(&all));

        s.reactor
            .handle_event(Event::SpaceChanged(vec![Some(space())], on_screen(&s, &all)));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![wid(2), wid(3)], s.parked());
        assert_eq!(vec![(wid(1), screen())], s.tiles());
    }

    /// R33.
    #[test]
    fn r33_a_space_change_to_none_from_the_login_window_changes_nothing() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        let frames = s.frames(&[wid(1), wid(2), wid(3)]);
        let journal = s.journal_on_disk();

        s.reactor.handle_event(Event::SpaceChanged(vec![None], Default::default()));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(frames, s.frames(&[wid(1), wid(2), wid(3)]));
        assert_eq!(vec![wid(2), wid(3)], s.parked());
        assert_eq!(journal, s.journal_on_disk());

        let snapshot = on_screen(&s, &[wid(1), wid(2), wid(3)]);
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space())], snapshot));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(frames, s.frames(&[wid(1), wid(2), wid(3)]));
        assert_eq!(vec![wid(2), wid(3)], s.parked());
        assert_eq!(vec![(wid(1), screen())], s.tiles());
    }

    /// R33, R7.
    #[test]
    fn r33_turning_off_one_space_shows_everything_only_there() {
        let left = screen();
        let right = rect(1200., 0., 1200., 1000.);
        let space2 = SpaceId::new(2);
        let mut s = Setup::on(vec![left, right], vec![Some(space()), Some(space2)]);
        let at = |x: f64, idx| WindowInfo {
            frame: rect(x, 100., 50., 50.),
            ..make_window(idx)
        };
        s.reactor.handle_events(
            s.apps.make_app(1, vec![at(100., 1), at(200., 2), at(1300., 3), at(1400., 4)]),
        );
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        let right_everything = s.frames(&[wid(3), wid(4)]);
        let c = s.create("C", &[wid(1), wid(3)]);
        s.switch(c);
        assert_eq!(vec![wid(2), wid(4)], s.parked());

        s.reactor.handle_event(Event::ShowEverythingOn(vec![space2]));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(vec![wid(2)], s.parked());
        assert_eq!(right_everything, s.frames(&[wid(3), wid(4)]));
        assert_eq!(right_everything, s.tiles_on(space2, right));
        assert_eq!(vec![(wid(1), left)], s.tiles_on(space(), left));
        assert_eq!(c, s.reactor.contexts.active());
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

    /// R6, L2.
    #[test]
    fn r6_deleting_the_active_context_shows_unsorted_first_without_errors() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1), wid(2)]);
        let d = s.create("D", &[wid(2)]);
        s.switch(d);
        s.switch(c);
        assert_eq!(vec![wid(3)], s.parked());
        let ContextKey::Named(c_id) = c else { unreachable!() };

        let errors = count_errors(|| s.reactor.delete_context(c_id).unwrap());
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(0, errors);
        assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
        assert_eq!(ContextKey::Unsorted, s.saved_active());
        assert!(s.reactor.layout.context_ids().all(|id| id != c_id));
        assert_eq!(vec![wid(2)], s.parked());
        let shown = vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(3), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(shown, s.tiles());
        assert_eq!(shown, s.frames(&[wid(1), wid(3)]));
    }

    /// R6, L2.
    #[test]
    fn r6_deleting_a_context_that_is_not_active_moves_nothing() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        s.switch(ContextKey::Everything);
        let frames = s.frames(&[wid(1), wid(2)]);
        let ContextKey::Named(c_id) = c else { unreachable!() };

        let errors = count_errors(|| s.reactor.delete_context(c_id).unwrap());

        assert_eq!(0, errors);
        assert!(s.apps.requests().is_empty());
        assert_eq!(frames, s.frames(&[wid(1), wid(2)]));
        assert_eq!(0, s.reactor.layout.context_ids().count());
        assert_eq!(ContextKey::Everything, s.saved_active());
    }

    /// L8. The app quits and starts again while another context is active,
    /// and its new window joins C there.
    #[test]
    fn l8_a_window_of_a_relaunched_app_is_tiled_when_its_context_becomes_active() {
        let mut s = Setup::new(1);
        let window = |idx: u32| WindowInfo {
            sys_id: Some(WindowServerId::new(20 + idx)),
            frame: rect(700., 100., 50., 50.),
            ..make_window(idx as usize)
        };
        s.reactor.handle_events(s.apps.make_app(2, vec![window(1)]));
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: on_screen(&s, &[wid(1), WindowId::new(2, 1)]),
        });
        s.apps.simulate_until_quiet(&mut s.reactor);
        let c = s.create("C", &[wid(1), WindowId::new(2, 1)]);
        let d = s.create("D", &[wid(1)]);
        s.switch(c);
        s.switch(d);
        assert_eq!(vec![WindowId::new(2, 1)], s.parked());

        s.reactor.handle_event(Event::ApplicationTerminated(2));
        s.reactor.handle_event(Event::ApplicationThreadTerminated(2));
        s.apps.windows.remove(&WindowId::new(2, 1));
        s.reactor.handle_events(s.apps.make_app(3, vec![window(1)]));
        let relaunched = WindowId::new(3, 1);
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: on_screen(&s, &[wid(1), relaunched]),
        });
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.add(c, relaunched);

        s.switch(c);

        let tiles = vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (relaunched, rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(tiles, s.tiles());
        assert_eq!(tiles, s.frames(&[wid(1), relaunched]));
        assert!(s.parked().is_empty());
    }

    /// L6.
    #[test]
    fn l6_a_minimized_member_leaves_the_layout_and_stays_a_member() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1), wid(2)]);
        s.switch(c);
        assert_eq!(vec![wid(3)], s.parked());

        // Window 2 is minimized.
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: on_screen(&s, &[wid(1), wid(3)]),
        });
        s.reactor.update_visible_windows();
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![(wid(1), screen())], s.tiles());
        s.switch(ContextKey::Everything);
        s.switch(c);
        assert_eq!(vec![(wid(1), screen())], s.tiles());
        assert_eq!(vec![wid(3)], s.parked());
        assert!(s.reactor.contexts.is_member(c, wid(2)));

        // It comes back.
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: on_screen(&s, &[wid(1), wid(2), wid(3)]),
        });
        s.reactor.update_visible_windows();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let tiles = vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(2), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(tiles, s.tiles());
        assert_eq!(tiles, s.frames(&[wid(1), wid(2)]));
    }

    /// L2, L8, L9. Floating a window under C leaves its node in Everything's
    /// layout for another screen size.
    #[test]
    fn l2_a_display_change_leaves_a_window_that_floats_out_of_the_new_sizes_layout() {
        let mut s = Setup::new(2);
        let shorter = rect(0., 0., 1200., 800.);
        let display = |s: &Setup, frame| match screens(vec![frame], vec![Some(space())]) {
            Event::ScreenParametersChanged {
                frames,
                bounds,
                ids,
                spaces,
                scale_factors,
                converter,
                ..
            } => Event::ScreenParametersChanged {
                frames,
                bounds,
                ids,
                spaces,
                scale_factors,
                converter,
                on_screen: on_screen(s, &[wid(1), wid(2)]),
            },
            _ => unreachable!(),
        };
        // Everything gets a layout of its own for each screen size.
        s.move_window(wid(1), Direction::Right);
        s.reactor.handle_event(display(&s, shorter));
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.move_window(wid(1), Direction::Left);
        s.reactor.handle_event(display(&s, screen()));
        s.apps.simulate_until_quiet(&mut s.reactor);
        let c = s.create("C", &[wid(1), wid(2)]);
        s.switch(c);
        float_window_1(&mut s);
        let floating = rect(100., 100., 50., 50.);
        s.switch(ContextKey::Everything);
        assert_eq!(vec![(wid(2), screen())], s.tiles());

        s.reactor.handle_event(display(&s, shorter));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(vec![(wid(2), shorter)], s.tiles_on(space(), shorter));
        assert_eq!(shorter, s.frame(wid(2)));
        assert_eq!(floating, s.frame(wid(1)));
    }

    /// R10, L5. A window server list that names no window the reactor knows,
    /// as right after the login window, takes no window out of the layout.
    #[test]
    fn a_space_change_with_an_incomplete_window_list_keeps_the_contexts_layout() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1), wid(2)]);
        s.switch(c);
        s.move_window(wid(1), Direction::Up);
        let arranged = vec![
            (wid(1), rect(0., 0., 1200., 500.)),
            (wid(2), rect(0., 500., 1200., 500.)),
        ];
        assert_eq!(arranged, s.tiles());

        s.reactor.handle_event(screens(vec![CGRect::ZERO], vec![None]));
        s.reactor.handle_event(screens(vec![screen()], vec![Some(space())]));
        assert_eq!(arranged, s.tiles());
        s.reactor.handle_event(Event::SpaceChanged(vec![None], Default::default()));
        s.reactor
            .handle_event(Event::SpaceChanged(vec![Some(space())], Default::default()));
        assert_eq!(arranged, s.tiles());
        let unmanaged = WindowServerInfo {
            id: WindowServerId::new(999),
            pid: 2,
            layer: 0,
            frame: rect(0., 0., 100., 100.),
        };
        s.reactor.handle_event(Event::SpaceChanged(
            vec![Some(space())],
            WindowsOnScreen::new(vec![unmanaged]),
        ));
        assert_eq!(arranged, s.tiles());

        // The full list arrives while the accessibility API still reports no
        // windows.
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: on_screen(&s, &[wid(1), wid(2), wid(3)]),
        });
        for request in s.apps.requests() {
            match request {
                Request::GetVisibleWindows => s.reactor.handle_event(Event::WindowsDiscovered {
                    pid: 1,
                    new: vec![],
                    known_visible: vec![],
                }),
                request => {
                    for event in s.apps.simulate_events_for_requests(vec![request]) {
                        s.reactor.handle_event(event);
                    }
                }
            }
        }
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(arranged, s.tiles());
        assert_eq!(arranged, s.frames(&[wid(1), wid(2)]));
        assert_eq!(vec![wid(3)], s.parked());
        assert_eq!(corner(CGSize::new(400., 1000.)), s.frame(wid(3)));
    }

    #[test]
    fn a_partial_ax_list_cannot_hide_a_visible_nonmember_during_a_switch() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1)]);
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: on_screen(&s, &[wid(1), wid(2)]),
        });
        s.reactor.handle_event(Event::WindowsDiscovered {
            pid: 1,
            new: vec![],
            known_visible: vec![wid(1)],
        });
        assert!(!s.reactor.visible_windows.contains(&WindowServerId::new(2)));

        let error = s.reactor.switch_context_on(None, c, None).unwrap_err();
        assert!(error.contains("AX has not confirmed"), "{error}");
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());

        s.reactor.handle_event(Event::WindowsDiscovered {
            pid: 1,
            new: vec![],
            known_visible: vec![wid(1), wid(2)],
        });
        assert!(s.reactor.visible_windows.contains(&WindowServerId::new(2)));
        s.switch(c);
        assert_eq!(c, s.reactor.contexts.active());
        assert_eq!(vec![wid(2)], s.parked());
    }

    #[test]
    fn an_initially_missing_ax_window_cannot_be_skipped_by_a_switch() {
        let mut s = Setup::on(vec![screen()], vec![Some(space())]);
        s.reactor.handle_events(s.apps.make_app(1, vec![]));
        s.reactor.handle_event(Event::StartupComplete);
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(vec![WindowServerInfo {
                id: WindowServerId::new(90),
                pid: 1,
                layer: 0,
                frame: rect(100., 100., 500., 500.),
            }]),
        });
        let c = ContextKey::Named(s.reactor.contexts.create("C").unwrap());

        let error = s.reactor.switch_context_on(None, c, None).unwrap_err();
        assert!(error.contains("AX has not confirmed"), "{error}");
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert!(s.journal_on_disk().is_empty());

        // On another Space neither source lists this window, so it cannot
        // prevent a switch there.
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::default(),
        });
        s.switch(c);
        assert_eq!(c, s.reactor.contexts.active());
    }

    #[test]
    fn a_window_server_row_before_app_registration_blocks_a_named_switch() {
        let mut s = Setup::on(vec![screen()], vec![Some(space())]);
        s.reactor.handle_event(Event::StartupComplete);
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(vec![WindowServerInfo {
                id: WindowServerId::new(90),
                pid: 2,
                layer: 0,
                frame: rect(100., 100., 500., 500.),
            }]),
        });
        let c = ContextKey::Named(s.reactor.contexts.create("C").unwrap());
        let error = s.reactor.switch_context_on(None, c, None).unwrap_err();
        assert!(error.contains("app 2"), "{error}");
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert!(s.journal_on_disk().is_empty());
    }

    #[test]
    fn startup_reapply_falls_back_when_ax_omits_a_visible_nonmember() {
        let mut s = Setup::on(vec![screen()], vec![Some(space())]);
        s.reactor.handle_events(s.apps.make_app(1, make_windows(2)));
        let c = s.create("C", &[wid(1)]);
        s.reactor.contexts.switch_to(c).unwrap();
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: on_screen(&s, &[wid(1), wid(2)]),
        });
        s.reactor.handle_event(Event::WindowsDiscovered {
            pid: 1,
            new: vec![],
            known_visible: vec![wid(1)],
        });
        assert!(!s.reactor.visible_windows.contains(&WindowServerId::new(2)));
        assert!(s.parked().is_empty());

        s.reactor.handle_event(Event::StartupComplete);
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
    }

    #[test]
    fn an_ax_list_on_another_space_does_not_hide_a_known_window() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(2)]);
        s.reactor.contexts.switch_to(c).unwrap();
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: on_screen(&s, &[wid(2)]),
        });
        s.reactor.handle_event(Event::WindowsDiscovered {
            pid: 1,
            new: vec![],
            known_visible: vec![wid(2)],
        });
        assert!(!s.reactor.hidden_windows.contains(&WindowServerId::new(1)));

        s.reactor.handle_event(Event::SpaceChanged(
            vec![Some(space())],
            on_screen(&s, &[wid(1)]),
        ));
        assert_eq!(c, s.reactor.contexts.active());
        assert_eq!(vec![wid(1)], s.parked());
    }

    #[test]
    fn a_closed_window_leaves_the_layout_and_no_longer_blocks_a_switch() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1), wid(2)]);
        s.switch(c);
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: on_screen(&s, &[wid(1), wid(2)]),
        });
        s.apps.windows.remove(&wid(2));
        s.reactor.handle_event(Event::WindowsDiscovered {
            pid: 1,
            new: vec![],
            known_visible: vec![wid(1)],
        });
        assert_eq!(vec![(wid(1), screen())], s.tiles());
        assert!(!s.reactor.lists_unsorted());

        s.reactor.apply_again();
        assert_eq!(c, s.reactor.contexts.active());

        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: on_screen(&s, &[wid(1)]),
        });
        s.switch(c);
        assert_eq!(c, s.reactor.contexts.active());
        assert!(s.parked().is_empty());
    }

    fn id_of(key: ContextKey) -> ContextId {
        let ContextKey::Named(id) = key else { panic!("{key:?}") };
        id
    }

    /// Reports the windows as the window server lists them, and asks every
    /// app for its visible windows, as the periodic refresh does.
    fn report_visible(s: &mut Setup, wids: &[WindowId]) {
        let snapshot = on_screen(s, wids);
        s.reactor
            .handle_event(Event::WindowsOnScreenUpdated { pid: None, on_screen: snapshot });
        s.reactor.update_visible_windows();
        s.apps.simulate_until_quiet(&mut s.reactor);
    }

    /// Sends `previous_context`, as a key binding does.
    fn previous(s: &mut Setup) {
        s.reactor
            .handle_event(Event::Command(Command::Context(ContextCommand::PreviousContext)));
    }

    /// The frame writes in `requests`, in order.
    fn all_frame_writes(requests: Vec<Request>) -> Vec<(WindowId, CGRect)> {
        requests
            .into_iter()
            .filter_map(|request| match request {
                Request::SetWindowFrame(wid, frame, _) => Some((wid, frame)),
                _ => None,
            })
            .collect()
    }

    /// R12, R13, R14, R27, H3, L6, L8. The sequence A, B, A, Everything, B
    /// with a floating window, tiled windows, a window in two contexts, a
    /// window in none, and a member that the user minimizes.
    #[test]
    fn m5a_a_b_a_everything_b_with_floating_tiled_shared_and_minimized_windows() {
        let mut s = Setup::new(5);
        float_window_1(&mut s);
        let floating = rect(100., 100., 50., 50.);
        assert_eq!(
            vec![
                (wid(2), rect(0., 0., 300., 1000.)),
                (wid(3), rect(300., 0., 300., 1000.)),
                (wid(4), rect(600., 0., 300., 1000.)),
                (wid(5), rect(900., 0., 300., 1000.)),
            ],
            s.tiles()
        );
        // Window 2 is in both contexts, and window 5 is in none.
        let a = s.create("A", &[wid(1), wid(2), wid(4)]);
        let b = s.create("B", &[wid(2), wid(3)]);

        s.switch(a);
        let in_a = vec![
            (wid(2), rect(0., 0., 600., 1000.)),
            (wid(4), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(in_a, s.tiles());
        assert_eq!(in_a, s.frames(&[wid(2), wid(4)]));
        assert_eq!(floating, s.frame(wid(1)));
        assert_eq!(vec![wid(3), wid(5)], s.parked());
        assert_eq!(corner(CGSize::new(300., 1000.)), s.frame(wid(3)));
        assert_eq!(corner(CGSize::new(300., 1000.)), s.frame(wid(5)));
        assert_eq!(
            vec![
                entry(3, rect(300., 0., 300., 1000.)),
                entry(5, rect(900., 0., 300., 1000.)),
            ],
            s.journal_on_disk()
        );

        // The user minimizes window 4.
        report_visible(&mut s, &[wid(1), wid(2), wid(3), wid(5)]);
        let minimized = rect(600., 0., 600., 1000.);
        assert_eq!(vec![(wid(2), screen())], s.tiles());
        assert_eq!(screen(), s.frame(wid(2)));
        assert_eq!(minimized, s.frame(wid(4)));

        s.switch(b);
        let in_b = vec![
            (wid(2), rect(0., 0., 600., 1000.)),
            (wid(3), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(in_b, s.tiles());
        assert_eq!(in_b, s.frames(&[wid(2), wid(3)]));
        assert_eq!(vec![wid(1), wid(5)], s.parked());
        assert_eq!(corner(floating.size), s.frame(wid(1)));
        assert_eq!(minimized, s.frame(wid(4)), "a minimized window is never parked");
        assert_eq!(
            vec![entry(5, rect(900., 0., 300., 1000.)), entry(1, floating)],
            s.journal_on_disk()
        );

        s.switch(a);
        assert_eq!(vec![(wid(2), screen())], s.tiles());
        assert_eq!(screen(), s.frame(wid(2)));
        assert_eq!(floating, s.frame(wid(1)));
        assert_eq!(vec![wid(3), wid(5)], s.parked());
        assert_eq!(corner(CGSize::new(600., 1000.)), s.frame(wid(3)));
        assert_eq!(minimized, s.frame(wid(4)));
        assert!(s.reactor.contexts.is_member(a, wid(4)));
        assert_eq!(
            vec![
                entry(5, rect(900., 0., 300., 1000.)),
                entry(3, rect(600., 0., 600., 1000.)),
            ],
            s.journal_on_disk()
        );

        s.switch(ContextKey::Everything);
        let everything = vec![
            (wid(2), rect(0., 0., 400., 1000.)),
            (wid(3), rect(400., 0., 400., 1000.)),
            (wid(5), rect(800., 0., 400., 1000.)),
        ];
        assert_eq!(everything, s.tiles());
        assert_eq!(everything, s.frames(&[wid(2), wid(3), wid(5)]));
        assert_eq!(floating, s.frame(wid(1)));
        assert_eq!(minimized, s.frame(wid(4)));
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());

        s.switch(b);
        assert_eq!(in_b, s.tiles());
        assert_eq!(in_b, s.frames(&[wid(2), wid(3)]));
        assert_eq!(vec![wid(1), wid(5)], s.parked());
        assert_eq!(corner(floating.size), s.frame(wid(1)));
        assert_eq!(corner(CGSize::new(400., 1000.)), s.frame(wid(5)));
        assert_eq!(minimized, s.frame(wid(4)));
        assert_eq!(
            vec![entry(1, floating), entry(5, rect(800., 0., 400., 1000.))],
            s.journal_on_disk()
        );
        assert_eq!(b, s.saved_active());
    }

    /// R6, L3. Unsorted's first layout is a copy of the layout that shows
    /// when the active context is deleted, so the windows stay in the order
    /// they had. That layout must still exist when Unsorted shows, so the
    /// deleted context's layouts go only after Unsorted shows.
    #[test]
    fn r6_deleting_the_active_context_keeps_the_arrangement_it_showed() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1), wid(2), wid(3)]);
        s.create("D", &[wid(3)]);
        s.switch(c);
        s.move_window(wid(1), Direction::Right);
        let in_c = vec![
            (wid(1), rect(400., 0., 400., 1000.)),
            (wid(2), rect(0., 0., 400., 1000.)),
            (wid(3), rect(800., 0., 400., 1000.)),
        ];
        assert_eq!(in_c, s.tiles());
        assert_eq!(in_c, s.frames(&[wid(1), wid(2), wid(3)]));

        let errors = count_errors(|| s.reactor.delete_context(id_of(c)).unwrap());
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(0, errors);
        assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
        // Window 3 is in D, so it is parked. Window 2 stays left of window 1,
        // as in C.
        let unsorted = vec![
            (wid(1), rect(600., 0., 600., 1000.)),
            (wid(2), rect(0., 0., 600., 1000.)),
        ];
        assert_eq!(unsorted, s.tiles());
        assert_eq!(unsorted, s.frames(&[wid(1), wid(2)]));
        assert_eq!(vec![wid(3)], s.parked());
        assert_eq!(vec![entry(3, rect(800., 0., 400., 1000.))], s.journal_on_disk());
        assert!(s.reactor.layout.context_ids().all(|id| id != id_of(c)));
    }

    /// R12, R30. A switch from one context to another whose journal write
    /// fails leaves the first context exactly as it was.
    #[test]
    fn r12_r30_a_failed_journal_write_from_c_to_d_keeps_c_as_it_was() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1), wid(2)]);
        let d = s.create("D", &[wid(2), wid(3)]);
        s.switch(c);
        let in_c = vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(2), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(in_c, s.tiles());
        let all = [wid(1), wid(2), wid(3)];
        let frames = s.frames(&all);
        let journal = s.journal_on_disk();
        assert_eq!(vec![entry(3, rect(800., 0., 400., 1000.))], journal);
        let saved = fs::read(s.dir.path().join("contexts.json")).unwrap();
        let txids = all.map(|wid| s.reactor.windows[&wid].last_sent_txid);
        let used = s.reactor.contexts.last_used(d);

        let failing = FailingWrites::start(s.dir.path());
        s.command(d);
        drop(failing);

        assert!(s.apps.requests().is_empty());
        assert_eq!(c, s.reactor.contexts.active());
        assert_eq!(Some(ContextKey::Everything), s.reactor.contexts.previous());
        assert_eq!(used, s.reactor.contexts.last_used(d));
        assert_eq!(vec![wid(3)], s.parked());
        assert_eq!(in_c, s.tiles());
        assert_eq!(frames, s.frames(&all));
        assert_eq!(txids, all.map(|wid| s.reactor.windows[&wid].last_sent_txid));
        assert!(s.reactor.layout.context_ids().all(|id| ContextKey::Named(id) == c));
        assert_eq!(journal, s.journal_on_disk());
        assert_eq!(saved, fs::read(s.dir.path().join("contexts.json")).unwrap());

        s.switch(d);
        assert_eq!(d, s.reactor.contexts.active());
        assert_eq!(Some(c), s.reactor.contexts.previous());
        let in_d = vec![
            (wid(2), rect(0., 0., 600., 1000.)),
            (wid(3), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(in_d, s.tiles());
        assert_eq!(in_d, s.frames(&[wid(2), wid(3)]));
        assert_eq!(vec![wid(1)], s.parked());
        assert_eq!(vec![entry(1, rect(0., 0., 600., 1000.))], s.journal_on_disk());
    }

    /// R12, steps 3 and 4. The frames that put the target's members back go
    /// out before the frames that park the other windows.
    #[test]
    fn r12_a_switch_puts_members_back_before_it_parks_the_others() {
        let mut s = Setup::new(4);
        let c = s.create("C", &[wid(1), wid(2)]);
        let d = s.create("D", &[wid(3), wid(4)]);
        s.switch(c);
        assert_eq!(vec![wid(3), wid(4)], s.parked());

        s.command(d);

        assert_eq!(
            vec![
                (wid(3), rect(0., 0., 600., 1000.)),
                (wid(4), rect(600., 0., 600., 1000.)),
                (wid(1), corner(CGSize::new(600., 1000.))),
                (wid(2), corner(CGSize::new(600., 1000.))),
            ],
            all_frame_writes(s.apps.requests())
        );
    }

    /// `switch_context` by number, by name with the switcher's ranking, and
    /// by id. Equal matches go to the most recently used context (R19). The
    /// reserved names name Everything, and Unsorted while it has a window.
    /// A reference that names no context changes nothing.
    #[test]
    fn switch_context_resolves_numbers_ranked_names_reserved_names_and_ids() {
        // Window 4 is in no context.
        let mut s = Setup::new(4);
        let comms = s.create("Comms", &[wid(1)]);
        let community = s.create("Community", &[wid(2)]);
        let client = s.create("Client work", &[wid(3)]);
        let send = |s: &mut Setup, reference: ContextRef| {
            s.reactor.handle_event(Event::Command(Command::Context(
                ContextCommand::SwitchContext(reference),
            )));
        };
        let run = |s: &mut Setup, reference: ContextRef| {
            send(s, reference);
            s.apps.simulate_until_quiet(&mut s.reactor);
            s.reactor.contexts.active()
        };
        let name = |text: &str| ContextRef::Name(text.into());

        assert_eq!(community, run(&mut s, ContextRef::Number(2)));
        assert_eq!(comms, run(&mut s, ContextRef::Number(1)));
        assert_eq!(comms, run(&mut s, name("comm")));
        assert_eq!(community, run(&mut s, name("community")));
        assert_eq!(community, run(&mut s, name("comm")));
        assert_eq!(client, run(&mut s, name("clïent")));
        assert_eq!(client, run(&mut s, name("CW")));
        assert_eq!(comms, run(&mut s, ContextRef::Id(id_of(comms))));
        assert_eq!(vec![wid(2), wid(3), wid(4)], s.parked());

        let unknown_id: ContextId = serde_json::from_value(serde_json::json!(99)).unwrap();
        for reference in [
            ContextRef::Number(0),
            ContextRef::Number(4),
            ContextRef::Number(10),
            ContextRef::Id(unknown_id),
            name(""),
            name("   "),
            name("-"),
            name("xyz"),
        ] {
            send(&mut s, reference.clone());
            assert!(s.apps.requests().is_empty(), "{reference:?}");
            assert_eq!(comms, s.reactor.contexts.active(), "{reference:?}");
        }

        assert_eq!(ContextKey::Everything, run(&mut s, name("EVERYTHING")));
        assert!(s.parked().is_empty());
        assert_eq!(ContextKey::Unsorted, run(&mut s, name(" unsorted ")));
        assert_eq!(vec![wid(1), wid(2), wid(3)], s.parked());
        assert_eq!(vec![(wid(4), screen())], s.tiles());
        assert_eq!(screen(), s.frame(wid(4)));
    }

    /// R18, R6. Deleting the active context keeps the context used before it
    /// as the previous context.
    #[test]
    fn r18_after_deleting_the_active_context_previous_goes_to_the_one_before() {
        let mut s = Setup::new(3);
        let a = s.create("A", &[wid(1)]);
        let b = s.create("B", &[wid(2)]);
        let c = s.create("C", &[wid(3)]);
        s.switch(a);
        s.switch(b);
        s.switch(c);

        s.reactor.delete_context(id_of(c)).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
        assert_eq!(Some(b), s.reactor.contexts.previous());
        let used = |key| s.reactor.contexts.last_used(key);
        assert!(used(ContextKey::Unsorted) > used(b), "R19");
        // Window 3 was only in C, so it is unsorted now.
        assert_eq!(vec![(wid(3), screen())], s.tiles());
        assert_eq!(vec![wid(1), wid(2)], s.parked());

        previous(&mut s);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(b, s.reactor.contexts.active());
        assert_eq!(Some(ContextKey::Unsorted), s.reactor.contexts.previous());
        assert_eq!(vec![(wid(2), screen())], s.tiles());
        assert_eq!(screen(), s.frame(wid(2)));
        assert_eq!(vec![wid(1), wid(3)], s.parked());

        previous(&mut s);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
        assert_eq!(Some(b), s.reactor.contexts.previous());
        assert_eq!(vec![(wid(3), screen())], s.tiles());
        assert_eq!(screen(), s.frame(wid(3)));
        assert_eq!(vec![wid(1), wid(2)], s.parked());
    }

    /// R18. Deleting the previous context leaves no previous context, and
    /// `previous_context` then changes nothing.
    #[test]
    fn r18_deleting_the_previous_context_leaves_no_previous_context() {
        let mut s = Setup::new(2);
        let a = s.create("A", &[wid(1)]);
        let b = s.create("B", &[wid(2)]);
        s.switch(a);
        s.switch(b);
        assert_eq!(Some(a), s.reactor.contexts.previous());

        s.reactor.delete_context(id_of(a)).unwrap();
        assert!(s.apps.requests().is_empty());
        assert_eq!(None, s.reactor.contexts.previous());
        previous(&mut s);

        assert!(s.apps.requests().is_empty());
        assert_eq!(b, s.reactor.contexts.active());
        assert_eq!(vec![wid(1)], s.parked());
        assert_eq!(vec![(wid(2), screen())], s.tiles());
        assert_eq!(b, s.saved_active());
    }

    /// R18, R6. When deleting the active context makes Unsorted active, a
    /// previous Unsorted is dropped, and a previous Everything stays.
    #[test]
    fn r18_deleting_the_active_context_drops_a_previous_unsorted_and_keeps_everything() {
        let mut s = Setup::new(3);
        let everything = s.frames(&[wid(1), wid(2), wid(3)]);
        let a = s.create("A", &[wid(1)]);
        let b = s.create("B", &[wid(2)]);
        s.switch(ContextKey::Unsorted);
        assert_eq!(vec![(wid(3), screen())], s.tiles());
        s.switch(a);
        assert_eq!(Some(ContextKey::Unsorted), s.reactor.contexts.previous());

        s.reactor.delete_context(id_of(a)).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
        assert_eq!(None, s.reactor.contexts.previous());
        // Window 1 joins Unsorted's layout, whose columns follow the order of
        // the windows' first frames.
        let unsorted = vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(3), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(unsorted, s.tiles());
        assert_eq!(unsorted, s.frames(&[wid(1), wid(3)]));
        previous(&mut s);
        assert!(s.apps.requests().is_empty());
        assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());

        s.switch(ContextKey::Everything);
        s.switch(b);
        s.reactor.delete_context(id_of(b)).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
        assert_eq!(Some(ContextKey::Everything), s.reactor.contexts.previous());
        previous(&mut s);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert_eq!(everything, s.frames(&[wid(1), wid(2), wid(3)]));
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
    }

    /// Answers the requests in order, as the apps do.
    fn answer(s: &mut Setup, requests: Vec<Request>) {
        for event in s.apps.simulate_events_for_requests(requests) {
            s.reactor.handle_event(event);
        }
    }

    /// The displays change to `frames`, which show `spaces`, and the window
    /// server lists `wids` at the frames their apps report.
    fn displays(
        s: &Setup,
        frames: Vec<CGRect>,
        spaces: Vec<Option<SpaceId>>,
        wids: &[WindowId],
    ) -> Event {
        Event::ScreenParametersChanged {
            ids: (1..=frames.len() as u32).map(ScreenId::new).collect(),
            bounds: frames.clone(),
            scale_factors: vec![1.0; frames.len()],
            frames,
            spaces,
            converter: CoordinateConverter::default(),
            on_screen: on_screen(s, wids),
        }
    }

    /// The journal entry of window `window` of app `pid`, whose window server
    /// id is `wsid`.
    fn journal_entry(pid: i32, wsid: u32, window: usize, frame: CGRect) -> JournalEntry {
        JournalEntry {
            pid,
            bundle_id: Some(format!("com.testapp{pid}")),
            window_server_id: WindowServerId::new(wsid),
            title: format!("Window{window}"),
            frame: frame.into(),
        }
    }

    fn right() -> CGRect {
        rect(1200., 0., 1200., 1000.)
    }

    /// Two displays side by side, showing Spaces 1 and 2. Apps 1 and 2 each
    /// have window 1 on the left display and window 2 on the right one. App
    /// 2's window server ids are 21 and 22.
    fn two_displays_two_apps() -> Setup {
        let mut s = Setup::on(
            vec![screen(), right()],
            vec![Some(space()), Some(SpaceId::new(2))],
        );
        let window = |idx: usize, wsid: u32, x: f64| WindowInfo {
            sys_id: Some(WindowServerId::new(wsid)),
            frame: rect(x, 100., 50., 50.),
            ..make_window(idx)
        };
        let apps = [
            (1, vec![window(1, 1, 100.), window(2, 2, 1300.)]),
            (2, vec![window(1, 21, 200.), window(2, 22, 1400.)]),
        ];
        // The window server lists every visible window each time.
        let listed = WindowsOnScreen::new(
            apps.iter()
                .flat_map(|(pid, windows)| {
                    windows.iter().map(|info| WindowServerInfo {
                        id: info.sys_id.unwrap(),
                        pid: *pid,
                        layer: 0,
                        frame: info.frame,
                    })
                })
                .collect(),
        );
        for (pid, windows) in apps {
            let mut launch = s.apps.make_app(pid, windows);
            for event in &mut launch {
                if let Event::WindowsOnScreenUpdated { on_screen, .. } = event {
                    *on_screen = listed.clone();
                }
            }
            s.reactor.handle_events(launch);
        }
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        s
    }

    /// Windows 1 and 2 of apps 1 and 2 in `two_displays_two_apps`.
    fn four_windows() -> [WindowId; 4] {
        [wid(1), wid(2), WindowId::new(2, 1), WindowId::new(2, 2)]
    }

    /// The frames of `four_windows` under Everything.
    fn four_windows_under_everything() -> Vec<(WindowId, CGRect)> {
        vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(2), rect(1200., 0., 600., 1000.)),
            (WindowId::new(2, 1), rect(600., 0., 600., 1000.)),
            (WindowId::new(2, 2), rect(1800., 0., 600., 1000.)),
        ]
    }

    /// R12, R14, H4, L4. App 2 launches while Everything shows, and a switch
    /// comes before app 2 has answered the frame that tiles its window. The
    /// window is parked, and the journal holds its tile. App 3 launches
    /// while the window server doesn't list its window yet, so that window
    /// is neither tiled nor parked.
    #[test]
    fn a_switch_while_an_app_is_launching_parks_its_window_at_its_first_tile() {
        let mut s = Setup::new(1);
        let c = s.create("C", &[wid(1)]);
        let launching = WindowId::new(2, 1);
        let first_frame = rect(700., 100., 50., 50.);
        let window = WindowInfo {
            sys_id: Some(WindowServerId::new(21)),
            frame: first_frame,
            ..make_window(1)
        };
        let mut launch = s.apps.make_app(2, vec![window]);
        for event in &mut launch {
            if let Event::WindowsOnScreenUpdated { on_screen, .. } = event {
                on_screen.info.insert(0, on_screen_info(&s, wid(1)));
                on_screen.visible.insert(0, WindowServerId::new(1));
            }
        }
        s.reactor.handle_events(launch);
        let tiling = s.apps.requests();
        let tile = rect(600., 0., 600., 1000.);
        assert_eq!(vec![tile], frame_writes(&tiling, launching));
        assert_eq!(first_frame, s.frame(launching));

        s.command(c);
        let switching = s.apps.requests();
        answer(&mut s, tiling);
        answer(&mut s, switching);
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(vec![(wid(1), screen())], s.tiles());
        assert_eq!(screen(), s.frame(wid(1)));
        assert_eq!(vec![launching], s.parked());
        assert_eq!(corner(tile.size), s.frame(launching));
        assert_eq!(vec![journal_entry(2, 21, 1, tile)], s.journal_on_disk());

        let unlisted = WindowId::new(3, 1);
        let unlisted_frame = rect(900., 100., 50., 50.);
        let window = WindowInfo {
            sys_id: Some(WindowServerId::new(31)),
            frame: unlisted_frame,
            ..make_window(1)
        };
        s.reactor
            .handle_events(s.apps.make_app_without_ws_info(3, vec![window], None, false));
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.switch(c);
        assert_eq!(unlisted_frame, s.frame(unlisted));
        assert_eq!(vec![launching], s.parked());
        assert_eq!(vec![(wid(1), screen())], s.tiles());

        s.switch(ContextKey::Everything);
        let everything = vec![(wid(1), rect(0., 0., 600., 1000.)), (launching, tile)];
        assert_eq!(everything, s.tiles());
        assert_eq!(everything, s.frames(&[wid(1), launching]));
        assert_eq!(unlisted_frame, s.frame(unlisted));
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
    }

    /// How the window server lists the window at the frame its app reports.
    fn on_screen_info(s: &Setup, wid: WindowId) -> WindowServerInfo {
        WindowServerInfo {
            id: s.reactor.windows[&wid].window_server_id.unwrap(),
            pid: wid.pid,
            layer: 0,
            frame: s.frame(wid),
        }
    }

    /// R10, L2, H4. The user changes Space before the apps have answered a
    /// switch's frames. The new Space shows the context, the late answers
    /// move nothing, and the first Space keeps the context's layout.
    #[test]
    fn r10_a_space_change_during_a_switch_shows_the_context_on_the_new_space() {
        let mut s = Setup::new(3);
        // Window 3 is on Space 2.
        report_visible(&mut s, &[wid(1), wid(2)]);
        assert_eq!(
            vec![
                (wid(1), rect(0., 0., 600., 1000.)),
                (wid(2), rect(600., 0., 600., 1000.)),
            ],
            s.tiles()
        );
        assert_eq!(rect(800., 0., 400., 1000.), s.frame(wid(3)));
        let c = s.create("C", &[wid(1), wid(3)]);

        s.command(c);
        let switching = s.apps.requests();
        let space2 = SpaceId::new(2);
        let snapshot = on_screen(&s, &[wid(3)]);
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space2)], snapshot));
        let changing = s.apps.requests();
        answer(&mut s, switching);
        answer(&mut s, changing);
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(vec![(wid(3), screen())], s.tiles_on(space2, screen()));
        assert_eq!(screen(), s.frame(wid(3)));
        assert_eq!(screen(), s.frame(wid(1)));
        assert_eq!(vec![wid(2)], s.parked());
        assert_eq!(corner(CGSize::new(600., 1000.)), s.frame(wid(2)));
        assert_eq!(vec![entry(2, rect(600., 0., 600., 1000.))], s.journal_on_disk());

        let snapshot = on_screen(&s, &[wid(1), wid(2)]);
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space())], snapshot));
        assert!(all_frame_writes(s.apps.requests()).is_empty());
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![(wid(1), screen())], s.tiles());
        assert_eq!(screen(), s.frame(wid(1)));
        assert_eq!(vec![wid(2)], s.parked());
        assert_eq!(corner(CGSize::new(600., 1000.)), s.frame(wid(2)));
    }

    /// L2, H1, H4. The display gets shorter before the apps have answered a
    /// switch's frames. The members are tiled for the new size, the parked
    /// window moves to the corner of the new size, and its journal entry
    /// keeps its frame from before parking.
    #[test]
    fn l2_a_display_change_during_a_switch_ends_at_the_new_sizes_frames() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(1), wid(2)]);
        let shorter = rect(0., 0., 1200., 800.);

        s.command(c);
        let switching = s.apps.requests();
        // The window server still lists the frames from before the switch.
        let event = displays(&s, vec![shorter], vec![Some(space())], &all);
        s.reactor.handle_event(event);
        let changing = s.apps.requests();
        answer(&mut s, switching);
        answer(&mut s, changing);
        s.apps.simulate_until_quiet(&mut s.reactor);

        let in_c = vec![
            (wid(1), rect(0., 0., 600., 800.)),
            (wid(2), rect(600., 0., 600., 800.)),
        ];
        assert_eq!(in_c, s.tiles_on(space(), shorter));
        assert_eq!(in_c, s.frames(&[wid(1), wid(2)]));
        assert_eq!(vec![wid(3)], s.parked());
        assert_eq!(rect(1199., 799., 400., 1000.), s.frame(wid(3)));
        assert_eq!(vec![entry(3, rect(800., 0., 400., 1000.))], s.journal_on_disk());

        s.reactor.handle_event(displays(&s, vec![screen()], vec![Some(space())], &all));
        s.apps.simulate_until_quiet(&mut s.reactor);
        let in_c = vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(2), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(in_c, s.tiles());
        assert_eq!(in_c, s.frames(&[wid(1), wid(2)]));
        assert_eq!(corner(CGSize::new(400., 1000.)), s.frame(wid(3)));
        s.switch(ContextKey::Everything);
        assert_eq!(everything, s.frames(&all));
        assert!(s.journal_on_disk().is_empty());
    }

    /// L2, H1. After a switch, each screen size keeps its own arrangement of
    /// the context, and Everything's layout doesn't change.
    #[test]
    fn l2_a_context_keeps_an_arrangement_per_screen_size_across_display_changes() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(1), wid(2)]);
        s.switch(c);
        s.move_window(wid(1), Direction::Right);
        let full_c = vec![
            (wid(1), rect(600., 0., 600., 1000.)),
            (wid(2), rect(0., 0., 600., 1000.)),
        ];
        assert_eq!(full_c, s.tiles());
        let shorter = rect(0., 0., 1200., 800.);
        let change_to = |s: &mut Setup, frame: CGRect| {
            let event = displays(s, vec![frame], vec![Some(space())], &all);
            s.reactor.handle_event(event);
            s.apps.simulate_until_quiet(&mut s.reactor);
        };

        change_to(&mut s, shorter);
        // The new size starts from the arrangement the context showed.
        let short_c = vec![
            (wid(1), rect(600., 0., 600., 800.)),
            (wid(2), rect(0., 0., 600., 800.)),
        ];
        assert_eq!(short_c, s.tiles_on(space(), shorter));
        assert_eq!(short_c, s.frames(&[wid(1), wid(2)]));
        assert_eq!(rect(1199., 799., 400., 1000.), s.frame(wid(3)));
        s.move_window(wid(1), Direction::Left);
        let short_c = vec![
            (wid(1), rect(0., 0., 600., 800.)),
            (wid(2), rect(600., 0., 600., 800.)),
        ];
        assert_eq!(short_c, s.frames(&[wid(1), wid(2)]));

        change_to(&mut s, screen());
        assert_eq!(full_c, s.tiles());
        assert_eq!(full_c, s.frames(&[wid(1), wid(2)]));
        assert_eq!(corner(CGSize::new(400., 1000.)), s.frame(wid(3)));
        change_to(&mut s, shorter);
        assert_eq!(short_c, s.tiles_on(space(), shorter));
        assert_eq!(short_c, s.frames(&[wid(1), wid(2)]));
        change_to(&mut s, screen());
        assert_eq!(vec![entry(3, rect(800., 0., 400., 1000.))], s.journal_on_disk());

        s.switch(ContextKey::Everything);
        assert_eq!(everything, s.frames(&all));
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
    }

    /// R7, H1. A switch changes both displays. Each window stays on its
    /// display, and each parked window keeps 1 point in a corner of its own
    /// display that doesn't reach into the other one.
    #[test]
    fn r7_a_switch_changes_both_displays_and_keeps_each_window_on_its_own() {
        let mut s = two_displays_two_apps();
        let all = four_windows();
        assert_eq!(four_windows_under_everything(), s.frames(&all));
        let [w1, w2, v1, v2] = all;
        let c = s.create("C", &[w1, v2]);
        let d = s.create("D", &[w2, v1]);
        let space2 = SpaceId::new(2);

        s.switch(c);
        assert_eq!(vec![(w1, screen())], s.tiles_on(space(), screen()));
        assert_eq!(vec![(v2, right())], s.tiles_on(space2, right()));
        assert_eq!(vec![(w1, screen()), (v2, right())], s.frames(&[w1, v2]));
        assert_eq!(vec![w2, v1], s.parked());
        assert_eq!(rect(2399., 999., 600., 1000.), s.frame(w2));
        assert_eq!(rect(-599., 999., 600., 1000.), s.frame(v1));
        // The journal lists the windows of the left display first.
        assert_eq!(
            vec![
                journal_entry(2, 21, 1, rect(600., 0., 600., 1000.)),
                entry(2, rect(1200., 0., 600., 1000.)),
            ],
            s.journal_on_disk()
        );

        s.switch(d);
        assert_eq!(vec![(v1, screen())], s.tiles_on(space(), screen()));
        assert_eq!(vec![(w2, right())], s.tiles_on(space2, right()));
        assert_eq!(vec![(w2, right()), (v1, screen())], s.frames(&[w2, v1]));
        assert_eq!(vec![w1, v2], s.parked());
        assert_eq!(rect(-1199., 999., 1200., 1000.), s.frame(w1));
        assert_eq!(rect(2399., 999., 1200., 1000.), s.frame(v2));
        assert_eq!(
            vec![entry(1, screen()), journal_entry(2, 22, 2, right())],
            s.journal_on_disk()
        );

        s.switch(ContextKey::Everything);
        assert_eq!(four_windows_under_everything(), s.frames(&all));
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
    }

    /// Splits `requests` into the requests to app 1 and those to app 2.
    fn by_app(requests: Vec<Request>) -> (Vec<Request>, Vec<Request>) {
        requests.into_iter().partition(|request| match request {
            Request::SetWindowFrame(wid, ..) => wid.pid == 1,
            other => panic!("{other:?}"),
        })
    }

    /// R32, R7. Quitting with windows parked on two displays puts each back
    /// on its own display, and quits once both apps have confirmed.
    #[test]
    fn r32_quitting_with_windows_parked_on_two_displays_waits_for_both_apps() {
        let mut s = two_displays_two_apps();
        let all = four_windows();
        let [w1, w2, v1, v2] = all;
        let c = s.create("C", &[w1, v2]);
        s.switch(c);
        assert_eq!(vec![w2, v1], s.parked());
        let exits = catch_exits(&mut s);

        save_and_exit(&mut s);

        let (app1, app2) = by_app(s.apps.requests());
        assert_eq!(
            vec![
                (w1, rect(0., 0., 600., 1000.)),
                (w2, rect(1200., 0., 600., 1000.))
            ],
            all_frame_writes(app1.iter().map(copy_request).collect())
        );
        assert_eq!(
            vec![
                (v1, rect(600., 0., 600., 1000.)),
                (v2, rect(1800., 0., 600., 1000.))
            ],
            all_frame_writes(app2.iter().map(copy_request).collect())
        );
        answer(&mut s, app1);
        assert!(exits.lock().unwrap().is_empty());
        assert_eq!(
            vec![journal_entry(2, 21, 1, rect(600., 0., 600., 1000.))],
            s.journal_on_disk()
        );
        answer(&mut s, app2);

        assert_eq!(vec![0], *exits.lock().unwrap());
        assert_eq!(four_windows_under_everything(), s.frames(&all));
        assert!(s.journal_on_disk().is_empty());
        assert_eq!(c, s.saved_active());
    }

    fn copy_request(request: &Request) -> Request {
        match request {
            Request::SetWindowFrame(wid, frame, txid) => {
                Request::SetWindowFrame(*wid, *frame, *txid)
            }
            other => panic!("{other:?}"),
        }
    }

    /// R32. Quitting with windows parked on two displays, when app 2 never
    /// confirms its window's frame. The quit happens at the deadline, and
    /// the journal keeps exactly app 2's entry.
    #[test]
    fn r32_quitting_on_two_displays_when_one_app_never_confirms_keeps_its_entry() {
        let mut s = two_displays_two_apps();
        let [w1, w2, v1, v2] = four_windows();
        let c = s.create("C", &[w1, v2]);
        s.switch(c);
        assert_eq!(vec![w2, v1], s.parked());
        let exits = catch_exits(&mut s);
        let start = Instant::now();

        save_and_exit(&mut s);
        let (app1, _app2) = by_app(s.apps.requests());
        answer(&mut s, app1);
        s.reactor.exit_deadline_tick(start + Duration::from_millis(1900));
        assert!(exits.lock().unwrap().is_empty());
        s.reactor.exit_deadline_tick(Instant::now() + Duration::from_secs(2));

        assert_eq!(vec![0], *exits.lock().unwrap());
        assert_eq!(
            vec![journal_entry(2, 21, 1, rect(600., 0., 600., 1000.))],
            s.journal_on_disk()
        );
        assert_eq!(rect(1200., 0., 600., 1000.), s.frame(w2));
        assert_eq!(rect(-599., 999., 600., 1000.), s.frame(v1));
        assert_eq!(c, s.saved_active());
        assert!(s.dir.path().join("layout.ron").exists());
        s.reactor.exit_deadline_tick(Instant::now() + Duration::from_secs(10));
        assert_eq!(vec![0], *exits.lock().unwrap(), "the quit happens once");
    }

    /// R32, step 2. While the quit waits, a Space change, a display change,
    /// and turning contexts off and on park nothing, and context commands
    /// do nothing. The quit happens once every window is back.
    #[test]
    fn r32_while_the_quit_waits_nothing_is_parked_and_context_commands_do_nothing() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(1)]);
        let d = s.create("D", &[wid(2)]);
        s.switch(c);
        assert_eq!(vec![wid(2), wid(3)], s.parked());
        let exits = catch_exits(&mut s);

        save_and_exit(&mut s);
        let mut requests = s.apps.requests();
        s.command(d);
        s.command(ContextKey::Everything);
        previous(&mut s);
        let snapshot = on_screen(&s, &all);
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space())], snapshot));
        s.reactor.handle_event(displays(&s, vec![screen()], vec![Some(space())], &all));
        s.reactor.handle_event(Event::ConfigChanged(config(false)));
        s.reactor.handle_event(Event::ConfigChanged(config(true)));
        requests.extend(s.apps.requests());

        let parking = CGPoint::new(1199., 999.);
        let writes = all_frame_writes(requests.iter().map(copy_or_keep).collect());
        assert!(
            writes.iter().all(|(_, frame)| frame.origin != parking),
            "{writes:?}"
        );
        assert!(exits.lock().unwrap().is_empty());
        answer_every_write(&mut s, requests);

        assert_eq!(vec![0], *exits.lock().unwrap());
        assert_eq!(everything, s.frames(&all));
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
        assert_eq!(c, s.reactor.contexts.active());
        assert_eq!(c, s.saved_active());
    }

    /// Answers the requests in order, and then every request that follows,
    /// as apps do: an app reports the frame after every write, even one
    /// that doesn't move the window. The test apps report only a change.
    fn answer_every_write(s: &mut Setup, mut requests: Vec<Request>) {
        while !requests.is_empty() {
            for request in requests {
                let echo = match &request {
                    Request::SetWindowFrame(wid, frame, txid) => Some((*wid, *frame, *txid)),
                    _ => None,
                };
                let mut events = s.apps.simulate_events_for_requests(vec![request]);
                if let Some((wid, frame, txid)) = echo
                    && events.is_empty()
                {
                    events.push(Event::WindowFrameChanged(
                        wid,
                        frame,
                        txid,
                        Requested(true),
                        None,
                    ));
                }
                for event in events {
                    s.reactor.handle_event(event);
                }
            }
            requests = s.apps.requests();
        }
    }

    fn copy_or_keep(request: &Request) -> Request {
        match request {
            Request::SetWindowFrame(wid, frame, txid) => {
                Request::SetWindowFrame(*wid, *frame, *txid)
            }
            _ => Request::GetVisibleWindows,
        }
    }

    /// R33, R28. Turning contexts off by a config reload shows every window
    /// in Everything's layout. While they are off, context commands and a
    /// Space change change nothing, and `contexts.json` isn't written.
    /// Turning them on again shows the active context as it was, with its
    /// floating member at its frame.
    #[test]
    fn r33_turning_contexts_off_and_on_by_config_reload_keeps_the_contexts_arrangement() {
        let mut s = Setup::new(4);
        float_window_1(&mut s);
        let floating = rect(100., 100., 50., 50.);
        let all = [wid(1), wid(2), wid(3), wid(4)];
        let tiled = [wid(2), wid(3), wid(4)];
        let everything = vec![
            (wid(2), rect(0., 0., 400., 1000.)),
            (wid(3), rect(400., 0., 400., 1000.)),
            (wid(4), rect(800., 0., 400., 1000.)),
        ];
        assert_eq!(everything, s.frames(&tiled));
        let c = s.create("C", &[wid(1), wid(2), wid(3)]);
        let d = s.create("D", &[wid(4)]);
        s.switch(d);
        s.switch(c);
        s.move_window(wid(2), Direction::Right);
        let in_c = vec![
            (wid(2), rect(600., 0., 600., 1000.)),
            (wid(3), rect(0., 0., 600., 1000.)),
        ];
        assert_eq!(in_c, s.tiles());
        assert_eq!(floating, s.frame(wid(1)));
        assert_eq!(vec![wid(4)], s.parked());
        let path = s.dir.path().join("contexts.json");
        let saved = fs::read(&path).unwrap();

        s.reactor.handle_event(Event::ConfigChanged(config(false)));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(everything, s.tiles());
        assert_eq!(everything, s.frames(&tiled));
        assert_eq!(floating, s.frame(wid(1)));
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
        for command in [
            ContextCommand::SwitchContext(ContextRef::Id(id_of(d))),
            ContextCommand::ShowEverything,
            ContextCommand::PreviousContext,
        ] {
            s.reactor.handle_event(Event::Command(Command::Context(command)));
            assert!(s.apps.requests().is_empty());
        }
        let snapshot = on_screen(&s, &all);
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space())], snapshot));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(everything, s.frames(&tiled));
        assert!(s.parked().is_empty());
        assert_eq!(c, s.reactor.contexts.active());
        assert_eq!(saved, fs::read(&path).unwrap());

        s.reactor.handle_event(Event::ConfigChanged(config(true)));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(in_c, s.tiles());
        assert_eq!(in_c, s.frames(&[wid(2), wid(3)]));
        assert_eq!(floating, s.frame(wid(1)));
        assert_eq!(vec![wid(4)], s.parked());
        assert_eq!(corner(CGSize::new(400., 1000.)), s.frame(wid(4)));
        assert_eq!(vec![entry(4, rect(800., 0., 400., 1000.))], s.journal_on_disk());
    }

    /// R33, H1, L2. The login window, and the screens coming back from it
    /// with window lists that name no window, move no window on either
    /// display. Neither does the full list that follows. The context keeps
    /// the arrangement the user made.
    #[test]
    fn r33_the_login_window_moves_no_window_on_either_display() {
        let mut s = two_displays_two_apps();
        let all = four_windows();
        let [w1, _, v1, v2] = all;
        let c = s.create("C", &[w1, v1, v2]);
        s.switch(c);
        s.move_window(v1, Direction::Left);
        let left = vec![
            (w1, rect(600., 0., 600., 1000.)),
            (v1, rect(0., 0., 600., 1000.)),
        ];
        assert_eq!(left, s.tiles_on(space(), screen()));
        let frames = s.frames(&all);
        let parked = s.parked();
        let journal = s.journal_on_disk();
        let space2 = SpaceId::new(2);
        let spaces = vec![Some(space()), Some(space2)];

        s.reactor
            .handle_event(screens(vec![CGRect::ZERO, CGRect::ZERO], vec![None, None]));
        s.reactor
            .handle_event(Event::SpaceChanged(vec![None, None], Default::default()));
        s.reactor.handle_event(screens(vec![screen(), right()], spaces.clone()));
        s.reactor.handle_event(Event::SpaceChanged(spaces, Default::default()));
        let snapshot = on_screen(&s, &all);
        s.reactor
            .handle_event(Event::WindowsOnScreenUpdated { pid: None, on_screen: snapshot });
        // The accessibility API still reports no windows.
        let mut writes = vec![];
        for request in s.apps.requests() {
            match request {
                Request::GetVisibleWindows => {
                    for pid in [1, 2] {
                        s.reactor.handle_event(Event::WindowsDiscovered {
                            pid,
                            new: vec![],
                            known_visible: vec![],
                        });
                    }
                }
                Request::SetWindowFrame(wid, frame, _) => writes.push((wid, frame)),
                other => panic!("{other:?}"),
            }
        }
        writes.extend(all_frame_writes(s.apps.requests()));

        assert_eq!(Vec::<(WindowId, CGRect)>::new(), writes);
        assert_eq!(frames, s.frames(&all));
        assert_eq!(parked, s.parked());
        assert_eq!(journal, s.journal_on_disk());
        assert_eq!(left, s.tiles_on(space(), screen()));
        assert_eq!(vec![(v2, right())], s.tiles_on(space2, right()));
    }

    /// R17. A switch writes each window's final frame at once, without an
    /// animation, even when animations are on.
    #[test]
    fn r17_a_switch_has_no_animation() {
        use super::super::animation::Message as AnimationMessage;

        let mut s = Setup::new(3);
        let mut config = Config::default();
        config.settings.default_disable = false;
        config.settings.animate = true;
        config.settings.experimental.contexts.enable = true;
        s.reactor.handle_event(Event::ConfigChanged(Arc::new(config)));
        let (animation_tx, mut animation_rx) = mpsc::unbounded_channel();
        s.reactor.animation_tx = Some(animation_tx);
        let c = s.create("C", &[wid(1), wid(2)]);
        let mut switch_without_animation = |s: &mut Setup, key: ContextKey| {
            s.command(key);
            let mut messages = 0;
            while let Ok(message) = animation_rx.try_recv() {
                messages += 1;
                match message {
                    AnimationMessage::SkipToEnd(animation) => animation.skip_to_end(),
                    AnimationMessage::Replace(_) => panic!("{key:?} animates"),
                }
            }
            assert!(messages > 0);
            s.apps.simulate_until_quiet(&mut s.reactor);
        };

        switch_without_animation(&mut s, c);
        let in_c = vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(2), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(in_c, s.frames(&[wid(1), wid(2)]));
        assert_eq!(corner(CGSize::new(400., 1000.)), s.frame(wid(3)));

        switch_without_animation(&mut s, ContextKey::Everything);
        assert_eq!(rect(800., 0., 400., 1000.), s.frame(wid(3)));
    }

    /// R16. A parked window that its app moves back on screen has drifted
    /// in, and switching to the active context parks it again. Its journal
    /// entry keeps the frame from before it was first parked.
    #[test]
    fn r16_switching_to_the_active_context_parks_a_window_its_app_moved_back() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        assert_eq!(vec![wid(2)], s.parked());
        let parked_at = corner(CGSize::new(600., 1000.));
        assert_eq!(parked_at, s.frame(wid(2)));
        // The app moves window 2 back on screen by itself.
        let moved = rect(300., 200., 600., 700.);
        let txid = s.reactor.windows[&wid(2)].last_sent_txid;
        s.apps.windows.get_mut(&wid(2)).unwrap().frame = moved;
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(2),
            moved,
            txid,
            Requested(false),
            None,
        ));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![(wid(1), screen())], s.tiles());

        s.switch(c);

        assert_eq!(parked_at, s.frame(wid(2)));
        assert_eq!(vec![wid(2)], s.parked());
        assert_eq!(vec![entry(2, rect(600., 0., 600., 1000.))], s.journal_on_disk());
        assert_eq!(vec![(wid(1), screen())], s.tiles());
    }

    /// R10, R16. A Space change applies the active context again, which
    /// parks again a parked window that its app moved back on screen. Only
    /// the window server's list says so; no frame change was reported.
    #[test]
    fn r10_r16_a_space_change_parks_again_a_window_its_app_moved_back() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        let parked_at = corner(CGSize::new(600., 1000.));
        assert_eq!(parked_at, s.frame(wid(2)));
        let moved = rect(300., 200., 600., 700.);
        s.apps.windows.get_mut(&wid(2)).unwrap().frame = moved;

        let snapshot = on_screen(&s, &[wid(1), wid(2)]);
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space())], snapshot));
        let requests = s.apps.requests();
        assert_eq!(vec![parked_at], frame_writes(&requests, wid(2)));
        answer(&mut s, requests);
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(parked_at, s.frame(wid(2)));
        assert_eq!(vec![wid(2)], s.parked());
        assert_eq!(vec![entry(2, rect(600., 0., 600., 1000.))], s.journal_on_disk());
        assert_eq!(vec![(wid(1), screen())], s.tiles());
    }

    /// L4, R39. Under a context, a window that isn't a member and becomes
    /// visible, here by being unminimized, gets no tile and is parked, and
    /// the members keep theirs.
    #[test]
    fn l4_r39_a_non_member_that_becomes_visible_under_a_context_is_parked() {
        let mut s = Setup::new(3);
        // Window 3 is minimized.
        report_visible(&mut s, &[wid(1), wid(2)]);
        let c = s.create("C", &[wid(1), wid(2)]);
        s.switch(c);
        let in_c = vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(2), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(in_c, s.tiles());
        assert!(s.parked().is_empty());

        report_visible(&mut s, &[wid(1), wid(2), wid(3)]);

        assert_eq!(in_c, s.tiles());
        assert_eq!(in_c, s.frames(&[wid(1), wid(2)]));
        assert_eq!(vec![wid(3)], s.parked());
        assert_eq!(corner(CGSize::new(400., 1000.)), s.frame(wid(3)));
        assert_eq!(vec![entry(3, rect(800., 0., 400., 1000.))], s.journal_on_disk());
    }

    /// L5, H4. A window list taken before a switch's frames landed, and a
    /// refresh right after it, change no tile, and the parked window stays
    /// parked.
    #[test]
    fn l5_a_window_list_from_before_the_switch_landed_changes_no_tile() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let before = on_screen(&s, &all);
        let c = s.create("C", &[wid(1), wid(2)]);

        s.command(c);
        let switching = s.apps.requests();
        s.reactor
            .handle_event(Event::WindowsOnScreenUpdated { pid: None, on_screen: before });
        s.reactor.update_visible_windows();
        let refreshing = s.apps.requests();
        answer(&mut s, switching);
        answer(&mut s, refreshing);
        s.apps.simulate_until_quiet(&mut s.reactor);

        let in_c = vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(2), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(in_c, s.tiles());
        assert_eq!(in_c, s.frames(&[wid(1), wid(2)]));
        assert_eq!(vec![wid(3)], s.parked());
        assert_eq!(corner(CGSize::new(400., 1000.)), s.frame(wid(3)));
        assert_eq!(vec![entry(3, rect(800., 0., 400., 1000.))], s.journal_on_disk());
    }

    /// R29, L1. Unsorted keeps its own layout across switches.
    #[test]
    fn r29_unsorted_keeps_its_own_arrangement_across_switches() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(1)]);
        s.switch(ContextKey::Unsorted);
        s.move_window(wid(2), Direction::Right);
        let unsorted = vec![
            (wid(2), rect(600., 0., 600., 1000.)),
            (wid(3), rect(0., 0., 600., 1000.)),
        ];
        assert_eq!(unsorted, s.tiles());
        assert_eq!(vec![wid(1)], s.parked());

        s.switch(c);
        assert_eq!(vec![(wid(1), screen())], s.tiles());
        s.switch(ContextKey::Everything);
        assert_eq!(everything, s.frames(&all));
        s.switch(ContextKey::Unsorted);

        assert_eq!(unsorted, s.tiles());
        assert_eq!(unsorted, s.frames(&[wid(2), wid(3)]));
        assert_eq!(vec![wid(1)], s.parked());
        assert_eq!(corner(CGSize::new(400., 1000.)), s.frame(wid(1)));
    }

    /// R30, R16. A journal failure during a Space change ends the context.
    /// Switching to it again after recovery can park its nonmembers.
    #[test]
    fn r30_a_failed_journal_write_in_a_space_change_parks_nothing() {
        let mut s = Setup::new(4);
        // Windows 3 and 4 are on Space 2.
        report_visible(&mut s, &[wid(1), wid(2)]);
        let on_space2 = [wid(3), wid(4)];
        let frames = s.frames(&on_space2);
        assert_eq!(
            vec![
                (wid(3), rect(600., 0., 300., 1000.)),
                (wid(4), rect(900., 0., 300., 1000.)),
            ],
            frames
        );
        let c = s.create("C", &[wid(1), wid(3)]);
        s.switch(c);
        assert_eq!(vec![wid(2)], s.parked());
        assert_eq!(vec![entry(2, rect(600., 0., 600., 1000.))], s.journal_on_disk());
        let space2 = SpaceId::new(2);

        let failing = FailingWrites::start(s.dir.path());
        let snapshot = on_screen(&s, &on_space2);
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space2)], snapshot));
        let requests = s.apps.requests();
        answer(&mut s, requests);
        s.apps.simulate_until_quiet(&mut s.reactor);
        drop(failing);

        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert!(s.parked().is_empty());

        let recovered_frame = s.frame(wid(4));
        s.switch(c);
        assert_eq!(vec![wid(4)], s.parked());
        assert_eq!(corner(recovered_frame.size), s.frame(wid(4)));
        assert_eq!(vec![(wid(3), screen())], s.tiles_on(space2, screen()));
        assert_eq!(vec![entry(4, recovered_frame)], s.journal_on_disk());
    }

    /// R13, R12. A switch to a context without windows parks every window
    /// and tiles nothing. With no app running, a switch changes only the
    /// active context.
    #[test]
    fn r13_a_switch_to_a_context_without_windows_parks_every_window() {
        let mut s = Setup::on(vec![screen()], vec![Some(space())]);
        let empty = s.create("Empty", &[]);
        s.switch(empty);
        assert_eq!(empty, s.reactor.contexts.active());
        assert_eq!(empty, s.saved_active());
        s.switch(ContextKey::Everything);
        s.reactor.handle_events(s.apps.make_app(1, make_windows(2)));
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        let all = [wid(1), wid(2)];
        let everything = s.frames(&all);

        s.switch(empty);

        assert!(s.tiles().is_empty());
        assert_eq!(vec![wid(1), wid(2)], s.parked());
        let parked_at = corner(CGSize::new(600., 1000.));
        assert_eq!(vec![(wid(1), parked_at), (wid(2), parked_at)], s.frames(&all));
        assert_eq!(
            vec![
                entry(1, rect(0., 0., 600., 1000.)),
                entry(2, rect(600., 0., 600., 1000.)),
            ],
            s.journal_on_disk()
        );
        s.switch(ContextKey::Everything);
        assert_eq!(everything, s.frames(&all));
        assert!(s.journal_on_disk().is_empty());
    }

    /// R33, R10. Between Sugarglider saying that it will stop managing a
    /// Space and the Space change that follows, a switch parks nothing
    /// there. When the Space is managed again, the context chosen in between
    /// shows.
    #[test]
    fn r33_a_switch_while_turning_off_parks_nothing_and_shows_after_turning_on() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(1)]);
        let d = s.create("D", &[wid(2)]);
        s.switch(c);

        s.reactor.handle_event(Event::ShowEverythingOn(vec![space()]));
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.switch(d);

        assert_eq!(d, s.reactor.contexts.active());
        assert_eq!(everything, s.frames(&all));
        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());

        s.reactor.handle_event(Event::SpaceChanged(vec![None], Default::default()));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(everything, s.frames(&all));
        let snapshot = on_screen(&s, &all);
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space())], snapshot));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(vec![(wid(2), screen())], s.tiles());
        assert_eq!(screen(), s.frame(wid(2)));
        assert_eq!(vec![wid(1), wid(3)], s.parked());
        assert_eq!(
            vec![
                entry(1, rect(0., 0., 400., 1000.)),
                entry(3, rect(800., 0., 400., 1000.)),
            ],
            s.journal_on_disk()
        );
    }

    impl Setup {
        /// A reactor with contexts on whose journal and `contexts.json` are
        /// in `dir`, read at `now` as the boot `boot`, with app 1's
        /// `windows` windows tiled side by side on one screen.
        fn in_dir(dir: TempDir, boot: Option<&str>, now: SystemTime, windows: usize) -> Setup {
            let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
            reactor.journal = ParkedJournal::open(dir.path().join("parked.json"), now);
            // Contexts are on first, so that `contexts.json` is read at `now`.
            reactor.handle_event(Event::ConfigChanged(config(true)));
            reactor.open_contexts(
                ContextsStore::new(dir.path().join("contexts.json")),
                boot.map(Into::into),
                now,
            );
            reactor.handle_event(screens(vec![screen()], vec![Some(space())]));
            let mut s = Setup {
                reactor,
                apps: Apps::new(),
                dir,
            };
            s.reactor.handle_events(s.apps.make_app(1, make_windows(windows)));
            s.reactor.handle_event(Event::StartupComplete);
            s.apps.simulate_until_quiet(&mut s.reactor);
            s
        }

        fn file_names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }

        fn saved_json(&self) -> serde_json::Value {
            serde_json::from_slice(&fs::read(self.dir.path().join("contexts.json")).unwrap())
                .unwrap()
        }
    }

    /// Journal and state files. An unreadable `contexts.json` is moved aside
    /// unchanged. Sugarglider starts with no contexts and shows every
    /// window, and the next save writes a new file.
    #[test]
    fn an_unreadable_contexts_json_is_moved_aside_and_every_window_shows() {
        let dir = TempDir::new().unwrap();
        let contents = br#"{ "version": 1, "contexts": [ { "id": 1, "na"#;
        fs::write(dir.path().join("contexts.json"), contents).unwrap();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);

        let mut s = Setup::in_dir(dir, Some("boot"), now, 3);

        let aside = "contexts.unreadable-1790000000.json";
        assert_eq!(vec![aside.to_string()], s.file_names());
        assert_eq!(contents.to_vec(), fs::read(s.dir.path().join(aside)).unwrap());
        assert!(s.reactor.contexts.contexts().is_empty());
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        let everything = vec![
            (wid(1), rect(0., 0., 400., 1000.)),
            (wid(2), rect(400., 0., 400., 1000.)),
            (wid(3), rect(800., 0., 400., 1000.)),
        ];
        assert_eq!(everything, s.tiles());
        assert_eq!(everything, s.frames(&[wid(1), wid(2), wid(3)]));
        assert!(s.parked().is_empty());

        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        assert_eq!(c, s.saved_active());
        assert_eq!(vec![wid(2), wid(3)], s.parked());
        assert_eq!(contents.to_vec(), fs::read(s.dir.path().join(aside)).unwrap());
    }

    /// Journal and state files, R4, R5. A `contexts.json` with a repeated id,
    /// names that differ only in case, a reserved name, a blank name,
    /// numbers out of range or taken, and an active context that doesn't
    /// exist loads repaired. Every window shows. Commands reach the repaired
    /// contexts by their new names and numbers, the reserved name still
    /// names the built-in entry, and the next save writes the repaired
    /// values.
    #[test]
    fn a_contexts_json_with_values_to_repair_loads_repaired() {
        let dir = TempDir::new().unwrap();
        let file = r#"{
            "version": 1, "next_id": 2, "use_seq": 5,
            "contexts": [
                { "id": 2, "name": "Work", "number": 2, "last_used": 4 },
                { "id": 2, "name": "WORK", "number": 2, "last_used": 5 },
                { "id": 3, "name": "unsorted", "number": 12 },
                { "id": 4, "name": "  ", "number": 0 }
            ],
            "active": { "global": 9 }
        }"#;
        fs::write(dir.path().join("contexts.json"), file).unwrap();

        let mut s = Setup::in_dir(dir, Some("boot"), SystemTime::now(), 3);

        let loaded: Vec<(u32, &str, Option<u8>)> = s
            .reactor
            .contexts
            .contexts()
            .iter()
            .map(|context| (context.id.get(), context.name.as_str(), context.number))
            .collect();
        assert_eq!(
            vec![
                (2, "Work", Some(2)),
                (5, "WORK 2", None),
                (3, "unsorted 2", None),
                (4, "Context 1", None),
            ],
            loaded
        );
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert!(s.parked().is_empty());
        assert_eq!(3, s.tiles().len());
        let run = |s: &mut Setup, reference: ContextRef| {
            s.reactor.handle_event(Event::Command(Command::Context(
                ContextCommand::SwitchContext(reference),
            )));
            s.apps.simulate_until_quiet(&mut s.reactor);
            s.reactor.contexts.active()
        };
        let named =
            |id: u32| ContextKey::Named(serde_json::from_value(serde_json::json!(id)).unwrap());
        assert_eq!(named(2), run(&mut s, ContextRef::Number(2)));
        assert_eq!(named(5), run(&mut s, ContextRef::Name("work 2".into())));
        assert_eq!(named(2), run(&mut s, ContextRef::Name("work".into())));
        assert_eq!(named(4), run(&mut s, ContextRef::Name("context".into())));
        assert_eq!(
            ContextKey::Unsorted,
            run(&mut s, ContextRef::Name("unsorted".into()))
        );
        assert_eq!(named(3), run(&mut s, ContextRef::Name("unsorted 2".into())));

        let saved = s.saved_json();
        assert_eq!(serde_json::json!(6), saved["next_id"]);
        let written: Vec<serde_json::Value> = saved["contexts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|context| serde_json::json!([context["id"], context["name"], context["number"]]))
            .collect();
        assert_eq!(
            vec![
                serde_json::json!([2, "Work", 2]),
                serde_json::json!([5, "WORK 2", null]),
                serde_json::json!([3, "unsorted 2", null]),
                serde_json::json!([4, "Context 1", null]),
            ],
            written
        );
        assert_eq!(serde_json::json!({ "global": 3 }), saved["active"]);
    }

    /// Journal and state files, R32. `contexts.json` names the active context
    /// by id, `"unsorted"`, or `"everything"`, and quitting writes it with
    /// the active context unchanged.
    #[test]
    fn contexts_json_names_the_active_context_and_quitting_writes_it() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1)]);
        s.switch(ContextKey::Unsorted);
        assert_eq!(
            serde_json::json!({ "global": "unsorted" }),
            s.saved_json()["active"]
        );
        s.switch(ContextKey::Everything);
        assert_eq!(
            serde_json::json!({ "global": "everything" }),
            s.saved_json()["active"]
        );
        s.switch(c);
        assert_eq!(
            serde_json::json!({ "global": id_of(c).get() }),
            s.saved_json()["active"]
        );
        let path = s.dir.path().join("contexts.json");
        fs::remove_file(&path).unwrap();
        let exits = catch_exits(&mut s);

        save_and_exit(&mut s);
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(vec![0], *exits.lock().unwrap());
        assert_eq!(
            serde_json::json!({ "global": id_of(c).get() }),
            s.saved_json()["active"]
        );
        assert_eq!(serde_json::json!("boot"), s.saved_json()["boot_id"]);
    }

    /// Journal and state files, R23. An app quits after its window closed,
    /// so its record is pending. `contexts.json` is written, with the
    /// window's last title.
    #[test]
    fn contexts_json_is_written_when_an_app_whose_record_is_pending_quits() {
        let mut s = Setup::new(1);
        let closing = WindowId::new(2, 1);
        let window = WindowInfo {
            sys_id: Some(WindowServerId::new(21)),
            ..make_window(1)
        };
        s.reactor.handle_events(s.apps.make_app(2, vec![window]));
        s.apps.simulate_until_quiet(&mut s.reactor);
        let c = s.create("C", &[wid(1), closing]);
        s.switch(c);
        let path = s.dir.path().join("contexts.json");
        fs::remove_file(&path).unwrap();
        // The window closes first, as the reactor records it.
        s.reactor.contexts.window_closed(closing);
        s.close(closing);
        assert!(!path.exists());

        s.reactor.handle_event(Event::ApplicationThreadTerminated(2));

        let members = &s.saved_json()["contexts"][0]["members"];
        assert_eq!(serde_json::json!("Window1"), members[1]["title"]);
        assert_eq!(serde_json::json!("com.testapp2"), members[1]["bundle_id"]);
    }

    /// Answers the requests until the apps are quiet, and adds each request
    /// to `trace`. Title tracking is left out: it is not a layout request,
    /// and only the run with contexts on has it.
    fn settle(reactor: &mut Reactor, apps: &mut Apps, trace: &mut Vec<String>) {
        loop {
            let requests = apps.requests();
            if requests.is_empty() {
                return;
            }
            trace.extend(
                requests
                    .iter()
                    .filter(|request| !matches!(request, Request::TrackTitles(_)))
                    .map(|request| format!("{request:?}")),
            );
            for event in apps.simulate_events_for_requests(requests) {
                reactor.handle_event(event);
            }
        }
    }

    /// The scenario of `it_moves_windows_dragged_between_spaces` and
    /// `it_preserves_layout_after_login_screen` in `reactor.rs`, followed by
    /// a display size change, a Space change, and the events and commands
    /// that contexts add. `on_launch` runs once the app has launched.
    /// Returns every request the reactor sent, in order, and the frames
    /// after the drag and at the end.
    fn run_drag_and_login_scenario(
        reactor: &mut Reactor,
        config: Arc<Config>,
        on_launch: impl FnOnce(&mut Reactor),
    ) -> (Vec<String>, Vec<CGRect>, Vec<CGRect>) {
        let mut apps = Apps::new();
        let mut trace = vec![];
        let screen1 = rect(0., 0., 1000., 1000.);
        let screen2 = rect(1000., 0., 1000., 1000.);
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let displays =
            |frames: Vec<CGRect>, spaces: Vec<Option<SpaceId>>| Event::ScreenParametersChanged {
                ids: vec![],
                scale_factors: vec![2.0; frames.len()],
                frames,
                bounds: vec![],
                spaces,
                converter: CoordinateConverter::default(),
                on_screen: Default::default(),
            };
        let frames = |apps: &Apps| [wid(1), wid(2)].map(|wid| apps.windows[&wid].frame).to_vec();
        reactor.handle_event(Event::ConfigChanged(config));
        reactor.handle_event(displays(
            vec![screen1, screen2],
            vec![Some(space1), Some(space2)],
        ));
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), Some(wid(1)), true));
        reactor.handle_event(Event::StartupComplete);
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        settle(reactor, &mut apps, &mut trace);
        on_launch(reactor);

        // The left display gets shorter, and the user changes the layout at
        // each size, so that Space 1 keeps a layout for each size.
        let short1 = rect(0., 0., 1000., 800.);
        let move_node = |reactor: &mut Reactor, direction| {
            reactor.handle_event(Event::Command(Command::Layout(LayoutCommand::MoveNode(
                direction,
            ))));
        };
        reactor.handle_event(displays(vec![short1, screen2], vec![Some(space1), Some(space2)]));
        settle(reactor, &mut apps, &mut trace);
        move_node(reactor, Direction::Up);
        settle(reactor, &mut apps, &mut trace);
        reactor.handle_event(displays(
            vec![screen1, screen2],
            vec![Some(space1), Some(space2)],
        ));
        settle(reactor, &mut apps, &mut trace);
        move_node(reactor, Direction::Right);
        settle(reactor, &mut apps, &mut trace);

        // The user drags window 1 onto the other screen.
        let dragged = wid(1);
        let frame = apps.windows[&dragged].frame;
        reactor.handle_event(Event::WindowFrameChanged(
            dragged,
            CGRect::new(CGPoint::new(1100., frame.origin.y), frame.size),
            apps.windows[&dragged].last_seen_txid,
            Requested(false),
            Some(MouseState::Down),
        ));
        settle(reactor, &mut apps, &mut trace);
        reactor.handle_event(Event::MouseUp);
        settle(reactor, &mut apps, &mut trace);
        let after_drag = frames(&apps);
        reactor.handle_event(displays(vec![short1, screen2], vec![Some(space1), Some(space2)]));
        settle(reactor, &mut apps, &mut trace);
        reactor.handle_event(displays(
            vec![screen1, screen2],
            vec![Some(space1), Some(space2)],
        ));
        settle(reactor, &mut apps, &mut trace);

        // The right display gets shorter and then its old size back.
        let shorter = rect(1000., 0., 1000., 800.);
        reactor.handle_event(displays(
            vec![screen1, shorter],
            vec![Some(space1), Some(space2)],
        ));
        settle(reactor, &mut apps, &mut trace);
        reactor.handle_event(displays(
            vec![screen1, screen2],
            vec![Some(space1), Some(space2)],
        ));
        settle(reactor, &mut apps, &mut trace);

        // The right display shows Space 3 and then Space 2 again.
        let listed = |apps: &Apps| {
            WindowsOnScreen::new(
                [(1, wid(1)), (2, wid(2))]
                    .map(|(id, wid)| WindowServerInfo {
                        id: WindowServerId::new(id),
                        pid: 1,
                        layer: 0,
                        frame: apps.windows[&wid].frame,
                    })
                    .to_vec(),
            )
        };
        let snapshot = listed(&apps);
        reactor.handle_event(Event::SpaceChanged(
            vec![Some(space1), Some(SpaceId::new(3))],
            snapshot,
        ));
        settle(reactor, &mut apps, &mut trace);
        let snapshot = listed(&apps);
        reactor.handle_event(Event::SpaceChanged(vec![Some(space1), Some(space2)], snapshot));
        settle(reactor, &mut apps, &mut trace);

        // The login window.
        reactor.handle_event(displays(vec![CGRect::ZERO, CGRect::ZERO], vec![None, None]));
        reactor.handle_event(displays(
            vec![screen1, screen2],
            vec![Some(space1), Some(space2)],
        ));
        let snapshot = listed(&apps);
        reactor.handle_event(Event::WindowsOnScreenUpdated { pid: None, on_screen: snapshot });
        let requests = apps.requests();
        trace.extend(requests.iter().map(|request| format!("{request:?}")));
        for request in requests {
            match request {
                Request::GetVisibleWindows => reactor.handle_event(Event::WindowsDiscovered {
                    pid: 1,
                    new: vec![],
                    known_visible: vec![],
                }),
                request => {
                    for event in apps.simulate_events_for_requests(vec![request]) {
                        reactor.handle_event(event);
                    }
                }
            }
        }
        settle(reactor, &mut apps, &mut trace);

        // The events and commands that contexts add.
        reactor.handle_event(Event::ShowEverythingOn(vec![space1, space2]));
        for command in [
            ContextCommand::SwitchContext(ContextRef::Number(1)),
            ContextCommand::SwitchContext(ContextRef::Name("C".into())),
            ContextCommand::ShowEverything,
            ContextCommand::PreviousContext,
        ] {
            reactor.handle_event(Event::Command(Command::Context(command)));
        }
        settle(reactor, &mut apps, &mut trace);
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_event(Event::Command(Command::Reactor(ReactorCommand::SaveAndExit)));
        settle(reactor, &mut apps, &mut trace);
        (trace, after_drag, frames(&apps))
    }

    /// R28. With contexts off, a scenario from the existing reactor tests
    /// sends exactly the same requests, and ends at the same frames, when
    /// the reactor holds an active context whose member is one of the
    /// windows, as it does after contexts were turned off. `contexts.json`
    /// isn't read, and no file is written.
    #[test]
    fn r28_with_contexts_off_a_saved_active_context_changes_no_request() {
        let mut plain = Reactor::new_for_test(LayoutManager::new_for_test());
        let (expected, after_drag, at_end) =
            run_drag_and_login_scenario(&mut plain, config(false), |_| {});
        let screen1 = rect(0., 0., 1000., 1000.);
        let screen2 = rect(1000., 0., 1000., 1000.);
        assert_eq!(vec![screen2, screen1], after_drag);
        assert_eq!(vec![screen2, screen1], at_end);

        let dir = TempDir::new().unwrap();
        let mut saved = crate::model::contexts::Contexts::new();
        let c = saved.create("C").unwrap();
        let desc = |idx: u32| WindowDesc {
            wid: wid(idx),
            bundle_id: Some("com.testapp1".into()),
            app_name: Some("TestApp1".into()),
            title: format!("Window{idx}"),
            window_server_id: Some(WindowServerId::new(idx)),
        };
        saved.add_window(c, &desc(1)).unwrap();
        saved.switch_to(ContextKey::Named(c)).unwrap();
        let store = ContextsStore::new(dir.path().join("contexts.json"));
        store.save(&saved, Some("boot")).unwrap();
        let file = fs::read(dir.path().join("contexts.json")).unwrap();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.journal = ParkedJournal::open(dir.path().join("parked.json"), SystemTime::now());
        reactor.open_contexts(store, Some("boot".into()), SystemTime::now());
        assert!(reactor.contexts.contexts().is_empty());
        reactor.contexts = saved;
        assert_eq!(ContextKey::Named(c), reactor.contexts.active());

        let (trace, after_drag, at_end) =
            run_drag_and_login_scenario(&mut reactor, config(false), |reactor| {
                reactor.contexts.add_window(c, &desc(1)).unwrap();
            });

        assert_eq!(expected, trace);
        assert_eq!(vec![screen2, screen1], after_drag);
        assert_eq!(vec![screen2, screen1], at_end);
        assert!(reactor.parked.is_empty());
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(vec!["contexts.json"], names);
        assert_eq!(file, fs::read(dir.path().join("contexts.json")).unwrap());
    }

    /// R28, L10. With contexts on and no context yet, the scenario sends
    /// exactly the requests it sends with contexts off. The window dragged
    /// to the other display leaves only the layout that Space 1 shows, so
    /// Space 1's layout for the shorter size keeps its place, as it does
    /// without contexts.
    #[test]
    fn r28_with_contexts_on_and_no_context_the_requests_are_those_with_contexts_off() {
        let mut plain = Reactor::new_for_test(LayoutManager::new_for_test());
        let (expected, ..) = run_drag_and_login_scenario(&mut plain, config(false), |_| {});
        let keeps_place = format!("SetWindowFrame({:?}, {:?}", wid(2), rect(0., 400., 1000., 400.));
        assert!(expected.iter().any(|request| request.starts_with(&keeps_place)));

        let dir = TempDir::new().unwrap();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.journal = ParkedJournal::open(dir.path().join("parked.json"), SystemTime::now());
        reactor.open_contexts(
            ContextsStore::new(dir.path().join("contexts.json")),
            Some("boot".into()),
            SystemTime::now(),
        );
        let (trace, after_drag, at_end) =
            run_drag_and_login_scenario(&mut reactor, config(true), |_| {});

        assert_eq!(expected, trace);
        let screen1 = rect(0., 0., 1000., 1000.);
        let screen2 = rect(1000., 0., 1000., 1000.);
        assert_eq!(vec![screen2, screen1], after_drag);
        assert_eq!(vec![screen2, screen1], at_end);
        assert!(reactor.contexts.contexts().is_empty());
        assert!(reactor.parked.is_empty());
    }

    /// R32, H3. Quitting puts a parked floating window back at the frame it
    /// had before it was parked.
    #[test]
    fn r32_quitting_puts_a_parked_floating_window_back_at_its_frame() {
        let mut s = Setup::new(2);
        float_window_1(&mut s);
        let floating = rect(100., 100., 50., 50.);
        let c = s.create("C", &[wid(2)]);
        s.switch(c);
        assert_eq!(vec![wid(1)], s.parked());
        assert_eq!(corner(floating.size), s.frame(wid(1)));
        let exits = catch_exits(&mut s);

        save_and_exit(&mut s);

        let requests = s.apps.requests();
        assert_eq!(vec![floating], frame_writes(&requests, wid(1)));
        answer(&mut s, requests);
        assert_eq!(vec![0], *exits.lock().unwrap());
        assert_eq!(floating, s.frame(wid(1)));
        assert_eq!(screen(), s.frame(wid(2)));
        assert!(s.journal_on_disk().is_empty());
    }

    /// R22, journal and state files. `contexts.json` from another boot keeps
    /// the contexts, their records, the active context, and their layouts,
    /// and forgets only the window server ids. The next save names the
    /// current boot.
    #[test]
    fn a_contexts_json_from_another_boot_keeps_everything_but_window_server_ids() {
        let mut s = Setup::new(1);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        s.switch(ContextKey::Everything);
        s.reactor.contexts.switch_to(c).unwrap();
        let store = ContextsStore::new(s.dir.path().join("contexts.json"));
        store.save(&s.reactor.contexts, Some("earlier boot")).unwrap();
        // No window is open, so none takes the record when the contexts are
        // read.
        s.close(wid(1));

        s.reactor.open_contexts(store, Some("boot".into()), SystemTime::now());

        assert_eq!(c, s.reactor.contexts.active());
        let context = &s.reactor.contexts.contexts()[0];
        assert_eq!(("C", Some(1)), (context.name.as_str(), context.number));
        let record = &context.members[0];
        assert_eq!(
            (Some("com.testapp1"), "Window1", None),
            (
                record.bundle_id.as_deref(),
                record.title.as_str(),
                record.window_server_id
            )
        );
        assert_eq!(
            vec![id_of(c)],
            s.reactor.layout.context_ids().collect::<Vec<_>>()
        );
        s.reactor.save_contexts();
        let saved = s.saved_json();
        assert_eq!(serde_json::json!("boot"), saved["boot_id"]);
        assert_eq!(
            None,
            saved["contexts"][0]["members"][0]
                .get("window_server_id")
                .filter(|id| !id.is_null())
        );
    }

    /// R12, R31. A switch that parks nothing needs no journal write, so it
    /// goes ahead when the journal can't be written. The windows come back,
    /// and the journal on disk loses their entries at the first retry after
    /// it can be written again. Retries come at most once a second.
    #[test]
    fn r31_a_switch_to_everything_goes_ahead_when_the_journal_cannot_be_written() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        let journal = s.journal_on_disk();
        assert_eq!(
            vec![
                entry(2, rect(400., 0., 400., 1000.)),
                entry(3, rect(800., 0., 400., 1000.)),
            ],
            journal
        );

        let failing = FailingWrites::start(s.dir.path());
        s.switch(ContextKey::Everything);
        drop(failing);

        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert_eq!(everything, s.frames(&all));
        assert_eq!(everything, s.tiles());
        assert!(s.parked().is_empty());
        assert!(s.reactor.journal.entries().is_empty());
        assert_eq!(journal, s.journal_on_disk());
        s.reactor.journal.retry_failed_write(Instant::now() + Duration::from_secs(1));
        assert!(s.journal_on_disk().is_empty());
    }

    /// R32, R31. A window that closes, and an app that ends, while the quit
    /// waits for them don't hold it up.
    #[test]
    fn r32_a_window_or_app_that_goes_away_during_the_quit_does_not_hold_it_up() {
        let mut s = Setup::new(3);
        let other = WindowId::new(2, 1);
        let window = WindowInfo {
            sys_id: Some(WindowServerId::new(21)),
            ..make_window(4)
        };
        s.reactor.handle_events(s.apps.make_app(2, vec![window]));
        report_visible(&mut s, &[wid(1), wid(2), wid(3), other]);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        assert_eq!(vec![wid(2), wid(3), other], s.parked());
        let exits = catch_exits(&mut s);

        save_and_exit(&mut s);
        let requests = s.apps.requests();
        let (window_3, _): (Vec<Request>, Vec<Request>) = requests.into_iter().partition(
            |request| matches!(request, Request::SetWindowFrame(target, ..) if *target == wid(3)),
        );
        answer(&mut s, window_3);
        assert!(exits.lock().unwrap().is_empty());
        s.close(wid(2));
        assert!(exits.lock().unwrap().is_empty());
        s.reactor.handle_event(Event::ApplicationThreadTerminated(2));

        assert_eq!(vec![0], *exits.lock().unwrap());
        assert!(s.journal_on_disk().is_empty());
        assert_eq!(c, s.saved_active());
    }

    /// R10, R7, L2, H1. A display added while a context is active shows the
    /// context in the same event: its member there gets a tile in the
    /// context's new layout for that Space, and the window there that isn't
    /// a member is parked in a corner of the new display.
    #[test]
    fn r10_a_display_added_under_a_context_shows_the_context_there_at_once() {
        let mut s = Setup::on(vec![screen()], vec![Some(space())]);
        let at = |idx: usize, x: f64| WindowInfo {
            frame: rect(x, 100., 50., 50.),
            ..make_window(idx)
        };
        s.reactor
            .handle_events(s.apps.make_app(1, vec![at(1, 100.), at(2, 1300.), at(3, 1400.)]));
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        report_visible(&mut s, &[wid(1)]);
        assert_eq!(vec![(wid(1), screen())], s.tiles());
        let c = s.create("C", &[wid(1), wid(2)]);
        s.switch(c);
        // Window 3 is on no screen, so there is nowhere to park it.
        assert!(s.parked().is_empty());
        let space2 = SpaceId::new(2);

        let all = [wid(1), wid(2), wid(3)];
        let event = displays(
            &s,
            vec![screen(), right()],
            vec![Some(space()), Some(space2)],
            &all,
        );
        s.reactor.handle_event(event);

        let requests = s.apps.requests();
        assert_eq!(vec![right()], frame_writes(&requests, wid(2)));
        assert_eq!(
            vec![rect(2399., 999., 50., 50.)],
            frame_writes(&requests, wid(3))
        );
        answer(&mut s, requests);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![(wid(1), screen())], s.tiles_on(space(), screen()));
        assert_eq!(vec![(wid(2), right())], s.tiles_on(space2, right()));
        assert_eq!(right(), s.frame(wid(2)));
        assert_eq!(vec![wid(3)], s.parked());
        assert_eq!(vec![entry(3, rect(1400., 100., 50., 50.))], s.journal_on_disk());
    }

    /// H2, L4. A parked window that becomes visible again, as the window
    /// server can report, gets no tile in the layout the Space shows.
    #[test]
    fn h2_a_parked_window_that_becomes_visible_gets_no_tile() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1), wid(2)]);
        s.switch(c);
        assert_eq!(vec![wid(3)], s.parked());
        let in_c = s.tiles();

        s.reactor.handle_event(Event::WindowBecameVisible(wid(3)));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(in_c, s.tiles());
        assert_eq!(in_c, s.frames(&[wid(1), wid(2)]));
        assert_eq!(vec![wid(3)], s.parked());
        s.switch(ContextKey::Everything);
        s.switch(c);
        assert_eq!(in_c, s.tiles());
    }

    /// H2, L4. A display change cannot leave a context active with visible
    /// nonmembers when their parking journal cannot be written.
    #[test]
    fn h2_a_failed_journal_on_display_change_ends_the_context() {
        let mut s = two_displays_two_apps();
        let space2 = SpaceId::new(2);
        let c = s.create("C", &[wid(1), wid(2)]);
        s.reactor.contexts.switch_to(c).unwrap();
        let failing = FailingWrites::start(s.dir.path());
        let all = four_windows();
        let event = displays(
            &s,
            vec![screen(), right()],
            vec![Some(space()), Some(space2)],
            &all,
        );
        s.reactor.handle_event(event);
        s.apps.simulate_until_quiet(&mut s.reactor);
        drop(failing);
        assert!(s.parked().is_empty());
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
    }

    /// L10. A window that the user moves to another display under a context
    /// leaves every layout of the Space it left, for every screen size and
    /// every context, and joins the context's layout on the other Space.
    #[test]
    fn l10_a_window_moved_to_another_display_leaves_every_layout_of_its_old_space() {
        let mut s = two_displays_two_apps();
        let space2 = SpaceId::new(2);
        let all = four_windows();
        let c = s.create("C", &all);
        s.switch(c);
        assert!(s.parked().is_empty());
        let shorter = rect(0., 0., 1200., 800.);
        for left in [shorter, screen()] {
            let event = displays(&s, vec![left, right()], vec![Some(space()), Some(space2)], &all);
            s.reactor.handle_event(event);
            s.apps.simulate_until_quiet(&mut s.reactor);
        }
        for key in [c, ContextKey::Everything] {
            assert!(s.reactor.layout.has_node_in(space(), key, wid(1)), "{key:?}");
        }

        let frame = rect(1500., 100., 600., 1000.);
        s.apps.windows.get_mut(&wid(1)).unwrap().frame = frame;
        let txid = s.reactor.windows[&wid(1)].last_sent_txid;
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            frame,
            txid,
            Requested(false),
            None,
        ));
        s.apps.simulate_until_quiet(&mut s.reactor);

        for key in [c, ContextKey::Everything] {
            assert!(!s.reactor.layout.has_node_in(space(), key, wid(1)), "{key:?}");
        }
        assert!(s.reactor.layout.has_node_in(space2, c, wid(1)));
        assert_eq!(
            vec![WindowId::new(2, 1)],
            s.tiles_on(space(), screen())
                .into_iter()
                .map(|(wid, _)| wid)
                .collect::<Vec<_>>()
        );
    }

    /// R32, R31. Showing Everything puts the windows back, and a quit comes
    /// before the app has moved them. The quit waits for them although no
    /// window is parked any more.
    #[test]
    fn r32_a_quit_right_after_showing_everything_waits_for_the_windows_it_put_back() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        assert_eq!(vec![wid(2), wid(3)], s.parked());
        let exits = catch_exits(&mut s);

        s.command(ContextKey::Everything);
        assert!(s.parked().is_empty());
        save_and_exit(&mut s);

        assert!(exits.lock().unwrap().is_empty());
        assert_eq!(2, s.journal_on_disk().len());
        let requests = s.apps.requests();
        assert_eq!(vec![everything[1].1], frame_writes(&requests, wid(2)));
        assert_eq!(vec![everything[2].1], frame_writes(&requests, wid(3)));
        answer(&mut s, requests);

        assert_eq!(vec![0], *exits.lock().unwrap());
        assert_eq!(everything, s.frames(&all));
        assert!(s.journal_on_disk().is_empty());
    }

    /// R32, R31. A switch puts back a window of app 2, and a quit comes
    /// before app 2 has moved it. The quit waits for app 2 as well as for the
    /// window it puts back itself.
    #[test]
    fn r32_a_quit_waits_for_a_window_that_a_switch_just_put_back() {
        let mut s = Setup::on(vec![screen()], vec![Some(space())]);
        let other = WindowId::new(2, 1);
        let window = WindowInfo {
            sys_id: Some(WindowServerId::new(30)),
            frame: rect(700., 100., 50., 50.),
            ..make_window(1)
        };
        s.reactor.handle_events(s.apps.make_app(1, make_windows(2)));
        s.reactor.handle_events(s.apps.make_app(2, vec![window]));
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: on_screen(&s, &[wid(1), wid(2), other]),
        });
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        let c = s.create("C", &[wid(1)]);
        let d = s.create("D", &[wid(1), other]);
        s.switch(c);
        assert_eq!(vec![wid(2), other], s.parked());
        let exits = catch_exits(&mut s);

        s.command(d);
        assert_eq!(vec![wid(2)], s.parked());
        save_and_exit(&mut s);
        assert!(exits.lock().unwrap().is_empty());
        let (app1, app2): (Vec<Request>, Vec<Request>) =
            s.apps.requests().into_iter().partition(|request| match request {
                Request::SetWindowFrame(wid, ..) => wid.pid == 1,
                _ => true,
            });
        assert!(!frame_writes(&app2, other).is_empty());
        answer(&mut s, app1);

        assert!(exits.lock().unwrap().is_empty());
        assert_eq!(
            vec![WindowServerId::new(30)],
            s.journal_on_disk()
                .iter()
                .map(|entry| entry.window_server_id)
                .collect::<Vec<_>>()
        );
        answer(&mut s, app2);
        assert_eq!(vec![0], *exits.lock().unwrap());
        assert!(s.journal_on_disk().is_empty());
        assert!(s.parked().is_empty());
    }

    /// Starts a reactor as `--restore` does, with the layout, the journal,
    /// and the contexts that `s` saved, and with contexts turned on as
    /// `contexts` says. No app has registered yet.
    fn restore(s: &Setup, contexts: bool) -> Reactor {
        let layout =
            LayoutManager::load(s.dir.path().join("layout.ron"), config(contexts)).unwrap();
        let mut reactor = Reactor::new_for_test(layout);
        reactor.journal = ParkedJournal::open(s.dir.path().join("parked.json"), SystemTime::now());
        reactor.open_contexts(
            ContextsStore::new(s.dir.path().join("contexts.json")),
            Some("boot".into()),
            SystemTime::now(),
        );
        reactor.handle_event(Event::ConfigChanged(config(contexts)));
        reactor.handle_event(screens(vec![screen()], vec![Some(space())]));
        reactor
    }

    /// App 1 registers with the reactor, with its windows at `frames`, and
    /// startup completes.
    fn register_app_1(reactor: &mut Reactor, frames: &[(WindowId, CGRect)]) -> Apps {
        let mut apps = Apps::new();
        let windows = frames
            .iter()
            .enumerate()
            .map(|(idx, &(_, frame))| WindowInfo { frame, ..make_window(idx + 1) })
            .collect();
        reactor.handle_events(apps.make_app(1, windows));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(reactor);
        apps
    }

    /// R32, R21, journal and state files. C is active, and its arrangement
    /// differs from the order of the windows under Everything. After a quit
    /// and `--restore`, the app's windows rejoin C when the app registers.
    /// C keeps its arrangement, and the window that isn't in C is parked at
    /// once. A Space change then changes nothing.
    #[test]
    fn r32_after_save_and_exit_and_restore_the_active_context_keeps_its_arrangement() {
        let mut s = Setup::new(4);
        let all = [wid(1), wid(2), wid(3), wid(4)];
        let c = s.create("C", &[wid(1), wid(2), wid(3)]);
        s.switch(c);
        s.move_window(wid(1), Direction::Right);
        let arranged = vec![
            (wid(1), rect(400., 0., 400., 1000.)),
            (wid(2), rect(0., 0., 400., 1000.)),
            (wid(3), rect(800., 0., 400., 1000.)),
        ];
        assert_eq!(arranged, s.tiles());
        let exits = catch_exits(&mut s);
        save_and_exit(&mut s);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![0], *exits.lock().unwrap());
        let everything = s.frames(&all);
        assert_eq!(rect(0., 0., 300., 1000.), everything[0].1);

        let mut reactor = restore(&s, true);
        assert_eq!(c, reactor.contexts.active());
        let tiles = |reactor: &Reactor| {
            let mut tiles = reactor.layout.calculate_layout(space(), screen(), &reactor.config);
            tiles.sort_by_key(|(wid, _)| *wid);
            tiles
        };
        assert_eq!(arranged, tiles(&reactor), "before any app registers");

        let mut apps = register_app_1(&mut reactor, &everything);

        let frames = |apps: &Apps, wids: &[WindowId]| {
            wids.iter().map(|&wid| (wid, apps.windows[&wid].frame)).collect::<Vec<_>>()
        };
        assert_eq!(arranged, tiles(&reactor));
        assert_eq!(arranged, frames(&apps, &all[..3]));
        assert_eq!(vec![wid(4)], reactor.parked.keys().copied().collect::<Vec<_>>());
        assert_eq!(corner(CGSize::new(300., 1000.)), apps.windows[&wid(4)].frame);
        assert_eq!(
            vec![entry(4, everything[3].1)],
            ParkedJournal::open(s.dir.path().join("parked.json"), SystemTime::now()).entries()
        );

        let snapshot = WindowsOnScreen::new(
            all.iter()
                .map(|&wid| WindowServerInfo {
                    id: reactor.windows[&wid].window_server_id.unwrap(),
                    pid: 1,
                    layer: 0,
                    frame: apps.windows[&wid].frame,
                })
                .collect(),
        );
        reactor.handle_event(Event::SpaceChanged(vec![Some(space())], snapshot));
        assert!(all_frame_writes(apps.requests()).is_empty());
        assert_eq!(arranged, tiles(&reactor));
        assert_eq!(vec![wid(4)], reactor.parked.keys().copied().collect::<Vec<_>>());
    }

    /// Names of the files in the directory that start with `prefix`.
    fn files_starting_with(dir: &TempDir, prefix: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with(prefix))
            .collect();
        names.sort();
        names
    }

    /// R28, journal and state files. With contexts off, the reactor doesn't
    /// read `contexts.json`. An unreadable file stays where it is, and a
    /// file with values to repair keeps its bytes. The layouts of contexts
    /// in `layout.ron` stay when `contexts.json` is missing, through a quit,
    /// and when contexts are turned on without a `contexts.json` to read.
    #[test]
    fn r28_with_contexts_off_contexts_json_is_not_read() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        let exits = catch_exits(&mut s);
        save_and_exit(&mut s);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![0], *exits.lock().unwrap());
        let everything = s.frames(&all);
        let path = s.dir.path().join("contexts.json");

        for contents in [
            &b"{ not json"[..],
            br#"{ "version": 1, "contexts": [ { "id": 4, "name": "Everything" } ] }"#,
        ] {
            fs::write(&path, contents).unwrap();
            let mut reactor = restore(&s, false);
            register_app_1(&mut reactor, &everything);
            assert_eq!(contents, &fs::read(&path).unwrap()[..]);
            assert!(files_starting_with(&s.dir, "contexts.unreadable").is_empty());
            assert!(reactor.contexts.contexts().is_empty());
        }

        fs::remove_file(&path).unwrap();
        let mut reactor = restore(&s, false);
        register_app_1(&mut reactor, &everything);
        assert_eq!(vec![id_of(c)], reactor.layout.context_ids().collect::<Vec<_>>());
        let exits = Arc::new(Mutex::new(vec![]));
        let caught = exits.clone();
        reactor.layout_file = Some(s.dir.path().join("layout.ron"));
        reactor.exit = Box::new(move |code| caught.lock().unwrap().push(code));
        reactor.handle_event(Event::Command(Command::Reactor(ReactorCommand::SaveAndExit)));
        assert_eq!(vec![0], *exits.lock().unwrap());
        assert!(!path.exists());
        let layout = LayoutManager::load(s.dir.path().join("layout.ron"), config(true)).unwrap();
        assert_eq!(vec![id_of(c)], layout.context_ids().collect::<Vec<_>>());

        // A reactor without a `contexts.json`, as in a replay.
        let mut reactor = Reactor::new_for_test(layout);
        reactor.handle_event(Event::ConfigChanged(config(true)));
        assert_eq!(vec![id_of(c)], reactor.layout.context_ids().collect::<Vec<_>>());
    }

    /// R33, R21, journal and state files. Contexts are off when Sugarglider
    /// starts, and a config reload turns them on after the app registered.
    /// `contexts.json` is read then, the windows rejoin C, and C shows with
    /// its arrangement while the window that isn't in C is parked.
    #[test]
    fn contexts_json_is_read_when_a_config_reload_turns_contexts_on() {
        let mut s = Setup::new(4);
        let all = [wid(1), wid(2), wid(3), wid(4)];
        let c = s.create("C", &[wid(1), wid(2), wid(3)]);
        s.switch(c);
        s.move_window(wid(1), Direction::Right);
        let arranged = vec![
            (wid(1), rect(400., 0., 400., 1000.)),
            (wid(2), rect(0., 0., 400., 1000.)),
            (wid(3), rect(800., 0., 400., 1000.)),
        ];
        assert_eq!(arranged, s.tiles());
        let exits = catch_exits(&mut s);
        save_and_exit(&mut s);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![0], *exits.lock().unwrap());
        let everything = s.frames(&all);
        let mut reactor = restore(&s, false);
        let mut apps = register_app_1(&mut reactor, &everything);
        assert!(reactor.contexts.contexts().is_empty());
        assert!(reactor.parked.is_empty());

        reactor.handle_event(Event::ConfigChanged(config(true)));
        apps.simulate_until_quiet(&mut reactor);

        assert_eq!(c, reactor.contexts.active());
        let mut tiles = reactor.layout.calculate_layout(space(), screen(), &reactor.config);
        tiles.sort_by_key(|(wid, _)| *wid);
        assert_eq!(arranged, tiles);
        assert_eq!(
            arranged,
            all[..3].iter().map(|&wid| (wid, apps.windows[&wid].frame)).collect::<Vec<_>>()
        );
        assert_eq!(vec![wid(4)], reactor.parked.keys().copied().collect::<Vec<_>>());
        assert_eq!(corner(CGSize::new(300., 1000.)), apps.windows[&wid(4)].frame);
    }

    /// An empty record of C that app 2's window titled "Window1" matches.
    fn record_of_app_2(s: &mut Setup, c: ContextKey) {
        let gone = WindowId::new(2, 9);
        let desc = WindowDesc {
            wid: gone,
            bundle_id: Some("com.testapp2".into()),
            app_name: Some("TestApp2".into()),
            title: "Window1".into(),
            window_server_id: None,
        };
        s.reactor.contexts.add_window(id_of(c), &desc).unwrap();
        s.reactor.contexts.window_closed(gone);
        s.reactor.contexts.app_terminated(2);
    }

    /// R28. After contexts are turned on and off again, an app that registers
    /// binds no record and has nothing applied to it.
    #[test]
    fn r28_an_app_that_registers_after_contexts_are_turned_off_rejoins_nothing() {
        let mut s = Setup::new(1);
        let c = s.create("C", &[wid(1)]);
        record_of_app_2(&mut s, c);
        s.switch(c);
        s.reactor.handle_event(Event::ConfigChanged(config(false)));
        s.apps.simulate_until_quiet(&mut s.reactor);

        let window = WindowInfo {
            sys_id: Some(WindowServerId::new(21)),
            frame: rect(700., 100., 50., 50.),
            ..make_window(1)
        };
        s.reactor.handle_events(s.apps.make_app(2, vec![window]));
        s.apps.simulate_until_quiet(&mut s.reactor);

        let arrived = WindowId::new(2, 1);
        assert!(!s.reactor.contexts.is_member(c, arrived));
        let members = &s.reactor.contexts.get(id_of(c)).unwrap().members;
        let links: Vec<RecordLink> = members.iter().map(|m| m.link).collect();
        assert_eq!(vec![RecordLink::Live(wid(1)), RecordLink::Empty], links);
        assert!(s.parked().is_empty());
        assert_eq!(
            vec![
                (wid(1), rect(0., 0., 600., 1000.)),
                (arrived, rect(600., 0., 600., 1000.)),
            ],
            s.frames(&[wid(1), arrived])
        );
    }

    /// Two screens that show Spaces 1 and 2. App 1 has windows 1 and 2 on
    /// the left screen and window 3 on the right one. C holds window 1, and
    /// windows 2 and 3 are parked. The right screen then moves to a Space
    /// that Sugarglider doesn't manage. Returns the frames the windows had
    /// under Everything.
    fn parked_on_a_screen_with_an_unmanaged_space() -> (Setup, Vec<(WindowId, CGRect)>) {
        let mut s = Setup::on(
            vec![screen(), right()],
            vec![Some(space()), Some(SpaceId::new(2))],
        );
        let at = |x: f64, idx| WindowInfo {
            frame: rect(x, 100., 50., 50.),
            ..make_window(idx)
        };
        s.reactor
            .handle_events(s.apps.make_app(1, vec![at(100., 1), at(200., 2), at(1300., 3)]));
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        let everything = s.frames(&[wid(1), wid(2), wid(3)]);
        assert_eq!(right(), everything[2].1);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        assert_eq!(vec![wid(2), wid(3)], s.parked());

        s.reactor.handle_event(Event::SpaceChanged(
            vec![Some(space()), None],
            on_screen(&s, &[wid(1), wid(2)]),
        ));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![wid(2), wid(3)], s.parked());
        assert_eq!(rect(2399., 999., 1200., 1000.), s.frame(wid(3)));
        (s, everything)
    }

    /// R27. Showing Everything puts back a window parked on a screen that
    /// now shows a Space Sugarglider doesn't manage.
    #[test]
    fn r27_everything_puts_back_a_window_parked_on_a_screen_with_an_unmanaged_space() {
        let (mut s, everything) = parked_on_a_screen_with_an_unmanaged_space();

        s.switch(ContextKey::Everything);

        assert!(s.parked().is_empty());
        assert_eq!(everything, s.frames(&[wid(1), wid(2), wid(3)]));
        assert!(s.journal_on_disk().is_empty());
    }

    /// R33. Turning Sugarglider off puts back a window parked on a screen
    /// that now shows a Space Sugarglider doesn't manage, as the space
    /// manager does it: it names the managed Spaces, and then reports them
    /// all as off.
    #[test]
    fn r33_turning_off_puts_back_a_window_parked_on_a_screen_with_an_unmanaged_space() {
        let (mut s, everything) = parked_on_a_screen_with_an_unmanaged_space();

        s.reactor.handle_event(Event::ShowEverythingOn(vec![space()]));
        s.reactor
            .handle_event(Event::SpaceChanged(vec![None, None], Default::default()));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert!(s.parked().is_empty());
        assert_eq!(everything, s.frames(&[wid(1), wid(2), wid(3)]));
        assert!(s.journal_on_disk().is_empty());
    }

    /// R33. Turning Sugarglider off while the user is on another Space puts
    /// back the windows parked on the Space the user left.
    #[test]
    fn r33_turning_off_from_another_space_puts_back_the_windows_parked_there() {
        let mut s = Setup::new(3);
        let everything = s.frames(&[wid(1), wid(2), wid(3)]);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        assert_eq!(vec![wid(2), wid(3)], s.parked());
        let space2 = SpaceId::new(2);
        s.reactor
            .handle_event(Event::SpaceChanged(vec![Some(space2)], Default::default()));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![wid(2), wid(3)], s.parked());

        s.reactor.handle_event(Event::ShowEverythingOn(vec![space2]));
        s.reactor.handle_event(Event::SpaceChanged(vec![None], Default::default()));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert!(s.parked().is_empty());
        assert!(s.journal_on_disk().is_empty());
        assert_eq!(everything[1..], s.frames(&[wid(2), wid(3)])[..]);
    }

    /// R28, R33. Turning contexts off puts back a window parked on a Space
    /// nobody sees and a window parked on a screen that shows a Space
    /// Sugarglider doesn't manage.
    #[test]
    fn r28_turning_contexts_off_puts_back_windows_parked_outside_the_visible_spaces() {
        let (mut s, everything) = parked_on_a_screen_with_an_unmanaged_space();
        // Window 2 leaves the visible Space, for example to be minimized.
        report_visible(&mut s, &[wid(1)]);
        assert_eq!(vec![wid(2), wid(3)], s.parked());

        s.reactor.handle_event(Event::ConfigChanged(config(false)));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert!(s.parked().is_empty());
        assert_eq!(everything[1..], s.frames(&[wid(2), wid(3)])[..]);
        assert!(s.journal_on_disk().is_empty());
    }

    /// R28. Turning contexts off while no screen shows a Space Sugarglider
    /// manages, as at the login window, puts back every parked window.
    #[test]
    fn r28_turning_contexts_off_with_no_managed_space_puts_back_every_parked_window() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        s.reactor.handle_event(Event::SpaceChanged(vec![None], Default::default()));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![wid(2), wid(3)], s.parked());

        s.reactor.handle_event(Event::ConfigChanged(config(false)));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert!(s.parked().is_empty());
        assert_eq!(everything[1..], s.frames(&[wid(2), wid(3)])[..]);
        assert!(s.journal_on_disk().is_empty());
    }

    fn switch_by_name(s: &mut Setup, name: &str) {
        s.reactor
            .handle_event(Event::Command(Command::Context(ContextCommand::SwitchContext(
                ContextRef::Name(name.into()),
            ))));
    }

    /// R28, R29. A name resolves to Unsorted only while a context exists and
    /// a window is in no context. Sugarglider's own windows don't count.
    /// Otherwise the switch is refused and nothing moves.
    #[test]
    fn a_name_resolves_to_unsorted_only_while_a_context_exists_and_unsorted_has_a_window() {
        let mut s = Setup::new(2);
        switch_by_name(&mut s, "Unsorted");
        assert!(s.apps.requests().is_empty());
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());

        let c = s.create("C", &[wid(1), wid(2)]);
        s.switch(c);
        let own_pid = std::process::id() as i32;
        let own_window = WindowInfo {
            sys_id: Some(WindowServerId::new(50)),
            ..make_window(3)
        };
        s.reactor.handle_events(s.apps.make_app(own_pid, vec![own_window]));
        let own = WindowId::new(own_pid, 1);
        report_visible(&mut s, &[wid(1), wid(2), own]);
        switch_by_name(&mut s, "unsorted");
        assert!(s.apps.requests().is_empty());
        assert_eq!(c, s.reactor.contexts.active());

        s.reactor.contexts.remove_window(id_of(c), wid(2)).unwrap();
        switch_by_name(&mut s, "unsorted");
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
        let tiled: Vec<WindowId> = s.tiles().into_iter().map(|(wid, _)| wid).collect();
        assert_eq!(vec![wid(2), own], tiled);
        assert_eq!(vec![wid(1)], s.parked());
    }

    /// R29. A window closed with ⌘W, which its app no longer lists but never
    /// reports destroyed, doesn't keep Unsorted listed.
    #[test]
    fn r29_a_window_closed_without_being_destroyed_doesnt_keep_unsorted_listed() {
        let mut s = Setup::new(3);
        let c = s.create("C", &[wid(1), wid(2)]);
        s.switch(c);
        assert_eq!(vec![wid(3)], s.parked());
        assert!(s.reactor.lists_unsorted());

        s.apps.windows.remove(&wid(3));
        s.reactor.handle_event(Event::WindowsDiscovered {
            pid: 1,
            new: vec![],
            known_visible: vec![wid(1), wid(2)],
        });

        assert!(s.reactor.windows.contains_key(&wid(3)));
        assert!(!s.reactor.lists_unsorted());
        switch_by_name(&mut s, "unsorted");
        assert!(s.apps.requests().is_empty());
        assert_eq!(c, s.reactor.contexts.active());
    }

    /// R29. A window the layout doesn't track, such as a floating panel, and
    /// a window of an app that quit don't keep Unsorted listed.
    #[test]
    fn r29_untracked_windows_and_windows_of_apps_that_quit_dont_keep_unsorted_listed() {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1), wid(2)]);
        s.switch(c);
        assert!(!s.reactor.lists_unsorted());
        let panel = WindowId::new(2, 1);
        let quitting = WindowId::new(3, 1);
        let at = |sys_id: u32| WindowInfo {
            sys_id: Some(WindowServerId::new(sys_id)),
            ..make_window(1)
        };
        // App 2 opens a panel, which reaches the reactor as a new window
        // does: `WindowCreated` first, then the window server's list, which
        // reports the panel's layer and so takes it out of the layout's
        // tracked windows.
        let info = at(20);
        s.apps.windows.insert(
            panel,
            WindowState {
                frame: info.frame,
                ..Default::default()
            },
        );
        s.reactor.handle_event(Event::WindowCreated(panel, info, MouseState::Up));
        s.reactor.handle_events(s.apps.make_app(3, vec![at(30)]));
        let mut listed = on_screen(&s, &[wid(1), wid(2), panel, quitting]);
        listed.info[2].layer = 3;
        s.reactor
            .handle_event(Event::WindowsOnScreenUpdated { pid: None, on_screen: listed });
        s.reactor.contexts.add_window(id_of(c), &s.desc(quitting)).unwrap();
        assert!(
            !s.reactor.contexts.is_member(c, panel),
            "the panel is in no context"
        );
        assert!(!s.reactor.lists_unsorted(), "only the panel is in no context");

        s.reactor.contexts.remove_window(id_of(c), quitting).unwrap();
        assert!(s.reactor.lists_unsorted(), "app 3's window is in no context");
        s.reactor.handle_event(Event::ApplicationThreadTerminated(3));
        assert!(!s.reactor.lists_unsorted());
    }

    /// Commands and dispatch, R29. A name resolves to Everything or Unsorted
    /// only by the entry's exact name. A partial name takes the named
    /// context it matches, even when a built-in entry was used more
    /// recently, and otherwise nothing.
    #[test]
    fn a_name_resolves_to_a_built_in_entry_only_by_its_exact_name() {
        let mut s = Setup::new(3);
        let unicorn = s.create("Unicorn", &[wid(1)]);
        s.switch(unicorn);
        s.switch(ContextKey::Unsorted);
        assert_eq!(vec![wid(1)], s.parked());

        switch_by_name(&mut s, "un");
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(unicorn, s.reactor.contexts.active());

        for partial in ["every", "unsort", "ev"] {
            switch_by_name(&mut s, partial);
            assert!(s.apps.requests().is_empty(), "{partial}");
            assert_eq!(unicorn, s.reactor.contexts.active(), "{partial}");
        }

        switch_by_name(&mut s, "UNSORTED");
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
        switch_by_name(&mut s, " everything ");
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert!(s.parked().is_empty());
    }

    /// R33. A display change that still lists a Space that is being turned
    /// off, between `ShowEverythingOn` for that Space and the Space change
    /// that turns it off, keeps showing Everything there. So no window is
    /// parked while Sugarglider is off. The space manager's display changes
    /// leave out a Space it is turning off, so this guards the reactor
    /// against another order. Turning the Space on again applies the context.
    #[test]
    fn r33_a_display_change_that_lists_a_space_being_turned_off_keeps_showing_everything() {
        let mut s = Setup::new(3);
        let all = [wid(1), wid(2), wid(3)];
        let everything = s.frames(&all);
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        s.reactor.handle_event(Event::ShowEverythingOn(vec![space()]));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert!(s.parked().is_empty());

        let event = displays(&s, vec![screen()], vec![Some(space())], &all);
        s.reactor.handle_event(event);
        s.reactor.handle_event(Event::SpaceChanged(vec![None], Default::default()));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert!(s.parked().is_empty());
        assert_eq!(everything, s.frames(&all));
        assert!(s.journal_on_disk().is_empty());

        let snapshot = on_screen(&s, &all);
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space())], snapshot));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![wid(2), wid(3)], s.parked());
        assert_eq!(vec![(wid(1), screen())], s.tiles());
    }
}

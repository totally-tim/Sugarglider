// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Focus from outside: when the user focuses a window of another context,
//! Sugarglider switches to that context. The design is in
//! `docs/specs/contexts.md`.

use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use super::Reactor;
use super::main_window::FocusSource;
use crate::actor::app::{Quiet, Request, WindowId, pid_t};
use crate::actor::layout::{EventResponse, LayoutEvent};
use crate::collections::HashSet;
use crate::model::contexts::{ContextKey, Scope};

const FINDER: &str = "com.apple.finder";

/// How long a switch waits at most for its end, in case the events that end it
/// never arrive, for example because an app didn't answer a write.
const GUARD_DEADLINE: Duration = Duration::from_secs(2);

/// What a switch waits for before focus from outside counts again. Until the
/// wait ends, activations and main window changes can be the switch's own.
#[derive(Debug, Default)]
pub(super) struct SwitchGuard {
    /// The raise sequence that focuses the switch's window, and the window.
    /// The wait for it starts when the raise manager reports that it sent the
    /// focusing raise: the failure or timeout of a batch before that raise
    /// doesn't end it.
    raise: Option<RaiseWait>,
    /// Windows awaiting verified parking echoes for the switch, including
    /// already-parked windows moved to a valid corner again.
    echoes: HashSet<WindowId>,
    /// Finder, when the switch activated it because no window could take focus,
    /// until its activation arrives or fails.
    pub(super) finder: Option<pid_t>,
    /// The last window that took focus while the guard held, to look at again
    /// when the guard ends. The window the switch focused itself doesn't
    /// count.
    pending_focus: Option<WindowId>,
    /// When the focus raise or Finder wait started. Parking echoes use their
    /// own deadlines.
    pub(super) since: Option<Instant>,
}

/// A switch's focusing raise, until its end arrives.
#[derive(Debug)]
struct RaiseWait {
    sequence_id: u64,
    focus: WindowId,
    /// Whether the raise manager has sent the focusing raise.
    sent: bool,
}

impl SwitchGuard {
    fn holds(&self) -> bool {
        self.raise.is_some() || !self.echoes.is_empty() || self.finder.is_some()
    }
}

/// What focus from outside did.
#[derive(Debug, PartialEq)]
enum FocusOutcome {
    /// The window keeps the focus, and the context stays.
    Stays,
    /// Sugarglider switched to a context that shows the window.
    Switched,
    /// Sugarglider raised a member of the active context instead.
    RaisedMember,
    /// Nothing happened, and the focus doesn't count.
    Ignored,
}

impl Reactor {
    /// Handles a window that took focus in a way that counts as the user's.
    /// While a switch is in progress it is ignored. A window the reactor hasn't
    /// seen yet waits until the reactor first sees it.
    pub(super) fn focus_changed(&mut self, wid: WindowId, source: FocusSource) {
        if !self.contexts_enabled() {
            return;
        }
        if self.switch_guard.holds() {
            // Look at the focus again when the switch ends, unless it is the
            // window the switch itself focused.
            if self.switch_guard.raise.as_ref().is_none_or(|raise| raise.focus != wid) {
                self.switch_guard.pending_focus = Some(wid);
            }
            debug!(
                ?wid,
                guard = ?self.switch_guard,
                "Ignoring focus while a switch is in progress"
            );
            return;
        }
        if !self.windows.contains_key(&wid) || self.pending_first_seen.contains(&wid) {
            debug!(?wid, "Focus on a window not seen yet waits for it");
            self.focus_waiting = Some((wid, source));
            return;
        }
        self.focus_waiting = None;
        if self.focus_from_outside(wid, source) == FocusOutcome::Stays {
            self.contexts.window_focused(wid);
        }
    }

    /// Applies focus that waited for windows the reactor sees for the first
    /// time, now that their membership is decided.
    pub(super) fn focus_windows_seen(&mut self, wids: &[WindowId]) {
        let Some((waiting, source)) = self.focus_waiting else {
            return;
        };
        if !wids.contains(&waiting) {
            return;
        }
        self.focus_waiting = None;
        if self.main_window() == Some(waiting) {
            self.focus_changed(waiting, source);
        }
    }

    /// When the user focuses a window that isn't a member of its screen's
    /// active context, switches to the most recently used context that holds
    /// it, or to Unsorted. When the user activates an app and the focused
    /// window is parked, and the app has a visible member of the active
    /// context, raises that member instead, and switches only when the app
    /// has no member there. A window focus change inside the frontmost app,
    /// such as ⌘`, is not an activation, so it switches.
    ///
    /// Windows Sugarglider doesn't track, its own windows, a screen that shows
    /// Everything, and focus before startup completes change nothing.
    fn focus_from_outside(&mut self, wid: WindowId, source: FocusSource) -> FocusOutcome {
        if !self.contexts_in_use() || self.pending_exit.is_some() || !self.startup_complete {
            return FocusOutcome::Stays;
        }
        let own_pid = std::process::id() as pid_t;
        let Some(info) = self.layout_window_info(wid) else {
            return FocusOutcome::Ignored;
        };
        if wid.pid == own_pid || self.layout.is_untracked(&info) {
            return FocusOutcome::Stays;
        }
        // R26. The screen the window was last shown on, which the switch
        // changes in `per_screen` scope.
        let screen = self.best_screen_idx_for_window(&info.frame);
        let shown = match screen {
            Some(screen) => self.screen_shown_key(screen),
            None => self.contexts.active(),
        };
        if self.shows_under(shown, wid) {
            return FocusOutcome::Stays;
        }
        if self.parked.contains_key(&wid) && source == FocusSource::Activation {
            if let Some(member) = self.visible_member_of_app(wid.pid, shown) {
                info!(?wid, ?member, "Raising the app's member of the active context");
                self.handle_layout_response(EventResponse {
                    focus_window: Some(member),
                    ..Default::default()
                });
                return FocusOutcome::RaisedMember;
            }
            let has_member = self
                .windows
                .keys()
                .any(|&other| other.pid == wid.pid && self.shows_under(shown, other));
            if has_member {
                debug!(?wid, "The app has a member of the active context; not switching");
                return FocusOutcome::Ignored;
            }
        }
        let target = self.contexts.focus_target_on(shown, self.membership_window(wid));
        let on = match self.scope() {
            Scope::Global => None,
            Scope::PerScreen => Some(screen.unwrap_or_else(|| self.focused_screen_index())),
        };
        info!(
            ?wid,
            ?target,
            ?on,
            "Focus from outside the active context; switching"
        );
        _ = self.switch_context_on(on, target, Some(wid));
        FocusOutcome::Switched
    }

    /// The most recently focused window of the app that is a member of `key`,
    /// is in the visible-window set, and isn't parked.
    fn visible_member_of_app(&self, pid: pid_t, key: ContextKey) -> Option<WindowId> {
        self.windows
            .iter()
            .filter(|&(&wid, window)| {
                wid.pid == pid
                    && !self.parked.contains_key(&wid)
                    && self.shows_under(key, wid)
                    && window
                        .window_server_id
                        .is_some_and(|wsid| self.visible_windows.contains(&wsid))
            })
            .map(|(&wid, _)| wid)
            .max_by_key(|&wid| (self.contexts.last_focus(wid), wid))
    }

    /// After windows were parked, handles the layout's `response` and raises
    /// `focus`, unless it is the main window already and `always` is false.
    /// When there is no window to focus, activates Finder instead. Focus from
    /// outside counts again when that ends.
    pub(super) fn focus_after_parking(
        &mut self,
        mut response: EventResponse,
        focus: Option<WindowId>,
        always: bool,
        parked: &[WindowId],
    ) {
        if let Some(focus) = focus
            && (always || self.main_window() != Some(focus))
        {
            response.focus_window = Some(focus);
        }
        let raised = response.focus_window;
        let sequence = self.handle_layout_response(response);
        let finder = match focus {
            Some(_) => None,
            None => self.activate_finder(),
        };
        self.guard_switch(sequence.zip(raised), parked, finder);
    }

    /// Starts waiting for the end of a switch: its focusing raise and the
    /// verified echo of every window it parked. With `finder`, the
    /// wait also lasts until Finder's activation arrives or fails. New focus
    /// actions get a fresh deadline; earlier parking echoes stay guarded.
    fn guard_switch(
        &mut self,
        raise: Option<(u64, WindowId)>,
        parked: &[WindowId],
        finder: Option<pid_t>,
    ) {
        let new_focus_wait = raise.is_some() || finder.is_some();
        let guard = &mut self.switch_guard;
        guard.echoes.extend(parked);
        match raise {
            Some((sequence_id, focus)) => {
                guard.raise = Some(RaiseWait {
                    sequence_id,
                    focus,
                    sent: false,
                });
            }
            None => {}
        }
        if finder.is_some() {
            guard.finder = finder;
        }
        if new_focus_wait {
            guard.since = Some(Instant::now());
        }
        debug!(guard = ?self.switch_guard, "Waiting for the switch to end");
    }

    pub(super) fn next_guard_deadline(&self) -> Option<Instant> {
        self.switch_guard
            .since
            .filter(|_| self.switch_guard.raise.is_some() || self.switch_guard.finder.is_some())
            .map(|since| since + GUARD_DEADLINE)
    }

    /// Ends the focus and Finder waits at their deadline. Parking echoes
    /// remain guarded until their own readback or parking deadline.
    pub(super) fn guard_deadline_tick(&mut self, now: Instant) {
        if self.next_guard_deadline().is_some_and(|deadline| now >= deadline) {
            let guard = &self.switch_guard;
            warn!(?guard, "The switch focus wait timed out");
            let awaiting = self.pending_parking_writes();
            let guard = &mut self.switch_guard;
            guard.raise = None;
            guard.echoes.retain(|wid| awaiting.contains_key(wid));
            guard.finder = None;
            guard.since = None;
            self.finish_guard();
        }
    }

    /// The raise manager sent the focusing raise of `sequence_id`. An
    /// identical request replaces the queued one with a newer id, so a
    /// sequence at least as new as the switch's is the switch's own raise.
    pub(super) fn raise_started(&mut self, sequence_id: u64) {
        let Some(raise) = &mut self.switch_guard.raise else {
            return;
        };
        if sequence_id >= raise.sequence_id {
            raise.sent = true;
            debug!(?sequence_id, "The switch's focusing raise is sent");
        }
    }

    /// A raise request failed for `windows`. The wait for the switch ends
    /// only when the focusing raise failed, once it was sent; the failure of
    /// another raise of the sequence doesn't.
    pub(super) fn raise_failed(&mut self, sequence_id: u64, windows: &[WindowId]) {
        let Some(raise) = &self.switch_guard.raise else { return };
        if sequence_id >= raise.sequence_id && raise.sent && windows.contains(&raise.focus) {
            self.switch_guard.raise = None;
            self.finish_guard();
        }
    }

    /// A raise sequence reported a completed raise of `window`, or with `None`,
    /// that it timed out. A sequence at least as new as the switch's ends the
    /// wait for it, because a request identical to the queued one replaces
    /// it. Until the focusing raise is sent, only a completed raise of the
    /// switch's window ends the wait: a timeout of the batch before the
    /// focusing raise doesn't.
    pub(super) fn raise_ended(&mut self, sequence_id: u64, window: Option<WindowId>) {
        let Some(raise) = &self.switch_guard.raise else { return };
        if sequence_id < raise.sequence_id {
            return;
        }
        let focuses_the_switch = match window {
            Some(window) => window == raise.focus,
            None => raise.sent,
        };
        if focuses_the_switch {
            self.switch_guard.raise = None;
            self.finish_guard();
        }
    }

    /// The echo of the last frame write to the window arrived.
    pub(super) fn frame_write_echoed(&mut self, wid: WindowId) {
        if self.switch_guard.echoes.remove(&wid) {
            self.finish_guard();
        }
    }

    /// A released window no longer has a parking write to wait for. The
    /// caller may still be building a new switch, so focus is settled after
    /// the current event instead of here.
    pub(super) fn cancel_parking_echo(&mut self, wid: WindowId) {
        self.switch_guard.echoes.remove(&wid);
    }

    pub(super) fn settle_released_parking_guard(&mut self) {
        if !self.switch_guard.holds()
            && (self.switch_guard.since.is_some() || self.switch_guard.pending_focus.is_some())
        {
            self.finish_guard();
        }
    }

    /// The app became active. Returns whether this ended the wait for Finder,
    /// whose activation a switch asked for.
    pub(super) fn app_activated(&mut self, pid: pid_t) -> bool {
        if self.switch_guard.finder != Some(pid) {
            return false;
        }
        self.switch_guard.finder = None;
        self.finish_guard();
        true
    }

    /// Stops waiting on a window that is gone.
    pub(super) fn guarded_window_gone(&mut self, wid: WindowId) {
        let guard = &mut self.switch_guard;
        let echoed = guard.echoes.remove(&wid);
        let raised = guard.raise.take_if(|raise| raise.focus == wid).is_some();
        if echoed || raised {
            self.finish_guard();
        }
    }

    /// Stops waiting on an app that is gone.
    pub(super) fn guarded_app_gone(&mut self, pid: pid_t) {
        let guard = &mut self.switch_guard;
        let before = guard.echoes.len();
        guard.echoes.retain(|wid| wid.pid != pid);
        let raised = guard.raise.take_if(|raise| raise.focus.pid == pid).is_some();
        let finder = guard.finder.take_if(|finder| *finder == pid).is_some();
        if raised || finder || guard.echoes.len() != before {
            self.finish_guard();
        }
    }

    /// The guard holds nothing any more. Focus that arrived while it held and
    /// is still the main window is looked at again now, as R24 does.
    fn finish_guard(&mut self) {
        if self.switch_guard.holds() {
            return;
        }
        self.switch_guard.since = None;
        debug!("The switch has ended; focus from outside counts again");
        let Some(pending) = self.switch_guard.pending_focus.take() else {
            return;
        };
        if self.main_window() == Some(pending) {
            debug!(?pending, "Applying focus that arrived during the switch");
            self.focus_changed(pending, FocusSource::MainWindowChange);
        }
    }

    /// When a switch leaves no window to focus, activates Finder quietly, so
    /// that keystrokes don't go to a parked window. Returns Finder's pid when
    /// its activation is to come.
    pub(super) fn activate_finder(&mut self) -> Option<pid_t> {
        let (&pid, app) = self
            .apps
            .iter()
            .find(|(_, app)| app.info.bundle_id.as_deref() == Some(FINDER))?;
        if self.main_window_tracker.frontmost_app() == Some(pid) {
            return None;
        }
        info!(pid, "No window can take focus; activating Finder");
        app.handle.send(Request::Activate(Quiet::Yes)).ok()?;
        Some(pid)
    }

    /// Selects the window in the layouts of the visible Spaces.
    pub(super) fn select_in_layout(&mut self, wid: WindowId) {
        let spaces = self.screens.iter().flat_map(|screen| screen.space).collect();
        self.send_layout_event(LayoutEvent::WindowFocused(spaces, wid));
    }
}

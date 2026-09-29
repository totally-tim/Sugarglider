// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Parking hides a window at a bottom corner of its screen, leaving no more
//! than a 1-by-32-point strip on that display.

use std::io;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CGRect, CGSize};
use tracing::{debug, error, info};

use super::animation::Animation;
use super::{MAX_REPARKS, Reactor, TransactionId, fit_frame_to_screen};
use crate::actor::app::{WindowId, pid_t};
use crate::actor::contexts_snapshot::RequestId;
use crate::actor::parked_journal::JournalEntry;
use crate::collections::{HashMap, HashSet};
use crate::model::{BottomCorner, accepted_bottom_strip, bounded_bottom_corner};
use crate::sys::app::Process;
use crate::sys::geometry::CGRectExt;
use crate::sys::screen::ScreenId;
use crate::sys::window_server::WindowServerId;

/// Finds the process that has a pid now.
pub(super) type ProcessLookup = Box<dyn Fn(pid_t) -> Process + Send>;

/// How far, in points, the position a window reports may be from the position
/// written to put it back, for the window to count as back.
const BACK_TOLERANCE: f64 = 16.0;
const PARK_CONFIRM_DEADLINE: Duration = Duration::from_secs(2);

/// Whether a window that reports `reported` is back at `target`. Only the
/// position counts, because some apps keep a size of their own.
fn is_back(reported: CGRect, target: CGRect) -> bool {
    (reported.origin.x - target.origin.x).abs() <= BACK_TOLERANCE
        && (reported.origin.y - target.origin.y).abs() <= BACK_TOLERANCE
}

/// Whether the bundle id in a journal entry and the bundle id of the app that
/// has the entry's pid now name the same app. Two missing bundle ids match; a
/// missing bundle id on one side only doesn't.
fn same_app(journal: &Option<String>, running: &Option<String>) -> bool {
    journal == running
}

/// A parked window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Parked {
    /// The frame the window had before it was parked, which is in its journal
    /// entry.
    pub(super) before: CGRect,
    /// Where the window was last seen: the corner written to it, or a frame
    /// that its app or the window server reported since. It never reaches the
    /// layout.
    pub(super) observed: CGRect,
    /// The display and window-server identity selected when the write began.
    screen: ScreenId,
    corner: BottomCorner,
    wsid: WindowServerId,
    /// A context parking write waiting for AX frame readback.
    awaiting: Option<(TransactionId, Instant)>,
}

pub(super) struct PendingParkingResult {
    pub(super) request: RequestId,
    pub(super) writes: HashMap<WindowId, TransactionId>,
}

/// A window whose journal entry is written, with the frame it has before it
/// is parked and the corner that parks it.
pub(super) struct Parking {
    wid: WindowId,
    frame: CGRect,
    corner: CGRect,
    screen: ScreenId,
    side: BottomCorner,
    wsid: WindowServerId,
}

impl Reactor {
    /// Parks each window in a corner of the screen it is on, and returns the
    /// windows it parked. See [`Reactor::journal_parking`] for the windows
    /// that are left where they are.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "switches park through journal_parking")
    )]
    pub(super) fn park_windows(&mut self, wids: &[WindowId]) -> io::Result<Vec<WindowId>> {
        let parking = self.journal_parking_with_policy(wids, false)?;
        Ok(self.move_to_corners(parking))
    }

    /// Writes the journal entries of the windows that can be parked, and
    /// returns where each goes. No window moves yet.
    ///
    /// If a visible window cannot be parked or the journal write fails,
    /// nothing changes. A window listed more than once is taken once.
    ///
    /// A window still at a parked corner or bottom strip keeps the original
    /// frame in its journal entry. Without an entry there is no safe frame to
    /// restore, so the park fails.
    pub(super) fn journal_parking(&mut self, wids: &[WindowId]) -> io::Result<Vec<Parking>> {
        self.journal_parking_with_policy(wids, true)
    }

    fn journal_parking_with_policy(
        &mut self,
        wids: &[WindowId],
        require_visible: bool,
    ) -> io::Result<Vec<Parking>> {
        let mut entries = vec![];
        let mut parking: Vec<Parking> = vec![];
        for &wid in wids {
            if self.parked.contains_key(&wid) || parking.iter().any(|p| p.wid == wid) {
                continue;
            }
            let Some(window) = self.windows.get(&wid) else {
                if require_visible {
                    return Err(io::Error::other(format!("Cannot find visible window {wid:?}")));
                }
                continue;
            };
            let Some(app) = self.apps.get(&wid.pid) else {
                if require_visible {
                    return Err(io::Error::other(format!(
                        "Cannot find app for visible window {wid:?}"
                    )));
                }
                continue;
            };
            let Some(wsid) = window.window_server_id else {
                if require_visible {
                    return Err(io::Error::other(format!(
                        "Cannot park visible window {wid:?} without a window-server id"
                    )));
                }
                continue;
            };
            if require_visible
                && (self.window_ids.get(&wsid) != Some(&wid)
                    || !self.visible_windows.contains(&wsid))
            {
                return Err(io::Error::other(format!(
                    "Cannot confirm the visible window-server identity of {wid:?}"
                )));
            }
            let current = window.frame_monotonic;
            if self
                .screens
                .iter()
                .all(|screen| screen.bounds.intersection(&current).area() == 0.0)
            {
                if require_visible {
                    return Err(io::Error::other(format!(
                        "Visible window {wid:?} has no frame on a display"
                    )));
                }
                continue;
            }
            let frame =
                if !self.shows_at_most_a_point(current) && !self.looks_like_parked_strip(current) {
                    current
                } else if let Some(entry) = self.journal.get(wid.pid, wsid) {
                    if !same_app(&entry.bundle_id, &app.info.bundle_id) {
                        return Err(io::Error::other(format!(
                            "Cannot use a parked-window journal entry from another app for {wid:?}"
                        )));
                    }
                    CGRect::from(entry.frame)
                } else {
                    if require_visible {
                        return Err(io::Error::other(format!(
                            "Cannot park {wid:?} without its original journal frame"
                        )));
                    }
                    continue;
                };
            let (corner, screen, side) = self.parking_target(frame).ok_or_else(|| {
                io::Error::other(format!("No clear bottom parking corner for {wid:?}"))
            })?;
            // Only the selected tab moves, but any identified tab can become
            // the window AX exposes after a selection, close, or restart.
            for member in self.tabs_of(wid) {
                entries.push(self.journal_entry(member, frame).ok_or_else(|| {
                    io::Error::other(format!("Cannot journal native tab {member:?}"))
                })?);
            }
            parking.push(Parking {
                wid,
                frame,
                corner,
                screen,
                side,
                wsid,
            });
        }
        if !parking.is_empty() {
            self.journal.record(entries)?;
        }
        Ok(parking)
    }

    /// The journal entry that puts the window back at `frame`.
    fn journal_entry(&self, wid: WindowId, frame: CGRect) -> Option<JournalEntry> {
        let window = self.windows.get(&wid)?;
        Some(JournalEntry {
            pid: wid.pid,
            bundle_id: self.apps.get(&wid.pid)?.info.bundle_id.clone(),
            window_server_id: window.window_server_id?,
            title: window.title.expose_secret().clone(),
            frame: frame.into(),
        })
    }

    /// Restores a group through its selected window when selection or merging
    /// changes the identity that owns a parked frame.
    pub(super) fn restore_selected_native_tab(
        &mut self,
        previous: WindowId,
        selected: WindowId,
        members: &[WindowId],
    ) {
        let inactive: Vec<_> = members
            .iter()
            .copied()
            .chain(std::iter::once(previous))
            .filter(|wid| *wid != selected && self.parked.contains_key(wid))
            .collect();
        let journal_frame = self
            .windows
            .get(&selected)
            .and_then(|window| window.window_server_id)
            .and_then(|wsid| self.journal.get(selected.pid, wsid))
            .map(|entry| CGRect::from(entry.frame));
        if inactive.is_empty() && (previous == selected || journal_frame.is_none()) {
            return;
        }
        let before = self
            .parked
            .get(&previous)
            .or_else(|| self.parked.get(&selected))
            .map(|parked| parked.before)
            .or(journal_frame)
            .or_else(|| {
                self.windows
                    .get(&previous)
                    .map(|window| window.frame_monotonic)
                    .filter(|frame| !self.looks_like_parked_strip(*frame))
            })
            .or_else(|| {
                inactive
                    .first()
                    .and_then(|wid| self.parked.get(wid))
                    .map(|parked| parked.before)
            });
        let Some(before) = before else { return };
        if let Some(entry) = self.journal_entry(selected, before) {
            if let Err(err) = self.journal.record(vec![entry]) {
                error!(
                    ?selected,
                    ?err,
                    "Could not record selected native tab; preserving the old journal and restoring Everything"
                );
            }
        }
        self.fail_parking_results_for(&inactive, "The selected native tab changed during parking");
        for wid in inactive {
            self.parked.remove(&wid);
            self.cancel_parking_echo(wid);
            self.moving_away.remove(&wid);
            self.frame_attempts.remove(&wid);
            self.forced_writes.remove(&wid);
            self.pending_frame_overrides.remove(&wid);
        }
        self.forced_writes.insert(selected);
        self.abort_failed_parking(
            "The selected native tab changed before restoration was confirmed".into(),
        );
        if !self.windows_in_layout().contains(&selected) {
            self.forced_writes.insert(selected);
            self.put_back_unplaced(&[(selected, before)]);
            self.update_layout(&[], true);
        }
    }

    /// Parks each parked window again when it isn't in the corner that parks
    /// it on the displays as they are now. A frame from before parking that
    /// is on no screen now moves onto a screen, and the window's journal
    /// entry changes before the window moves. If any target, retry cap, or
    /// journal write fails, the context ends and parked windows are restored.
    /// While no screen shows a managed Space, as at the login window, nothing
    /// moves.
    pub(super) fn repark_moved_windows(&mut self) -> Result<(), String> {
        if self.screens.iter().all(|screen| screen.space.is_none()) {
            return Ok(());
        }
        let mut wids: Vec<WindowId> = self.parked.keys().copied().collect();
        wids.sort();
        let mut entries = vec![];
        let mut moves = vec![];
        for wid in wids {
            let parked = self.parked[&wid];
            let before = self.on_a_screen(parked.before, parked.observed);
            let Some((corner, screen, side)) = self.parking_target(before) else {
                let reason = format!("No clear bottom parking corner for {wid:?}");
                self.abort_failed_parking(reason.clone());
                return Err(reason);
            };
            if before == parked.before
                && screen == parked.screen
                && side == parked.corner
                && self.accepts_parked_frame(parked.observed, corner.size, screen, side)
            {
                continue;
            }
            if self.repark_counts.get(&wid).copied().unwrap_or_default() >= MAX_REPARKS {
                let reason = format!("Window {wid:?} exceeded the parked-frame retry limit");
                self.abort_failed_parking(reason.clone());
                return Err(reason);
            }
            let identity_matches = self.windows.get(&wid).is_some_and(|window| {
                window.window_server_id == Some(parked.wsid)
                    && self.window_ids.get(&parked.wsid) == Some(&wid)
            }) && self.apps.get(&wid.pid).is_some_and(|app| {
                self.journal.get(wid.pid, parked.wsid).is_some_and(|entry| {
                    same_app(&entry.bundle_id, &app.info.bundle_id)
                        && CGRect::from(entry.frame) == parked.before
                })
            });
            if !identity_matches {
                let reason = format!(
                    "Cannot repark window {wid:?} without its original identity and journal frame"
                );
                self.abort_failed_parking(reason.clone());
                return Err(reason);
            }
            if before != parked.before {
                for member in self.tabs_of(wid) {
                    let Some(entry) = self.journal_entry(member, before) else {
                        let reason = format!("Cannot journal moved parked window {member:?}");
                        self.abort_failed_parking(reason.clone());
                        return Err(reason);
                    };
                    entries.push(entry);
                }
            }
            moves.push((wid, before, corner, screen, side));
        }
        if !entries.is_empty()
            && let Err(err) = self.journal.record(entries)
        {
            let reason = format!("Could not write the moved parked-window journal: {err}");
            self.abort_failed_parking(reason.clone());
            return Err(reason);
        }
        let mut writes = vec![];
        for (wid, before, corner, screen, side) in moves {
            *self.repark_counts.entry(wid).or_default() += 1;
            let parked = self.parked.get_mut(&wid).expect("the window is parked");
            if before != parked.before {
                parked.before = before;
            }
            parked.screen = screen;
            parked.corner = side;
            parked.observed = corner;
            writes.push((wid, corner));
        }
        if !writes.is_empty() {
            info!(count = writes.len(), "Parking windows again in valid corners");
            self.write_frames_now(&writes, true);
        }
        Ok(())
    }

    /// Moves windows whose journal entries are written into their corners,
    /// and returns them.
    pub(super) fn move_to_corners(&mut self, parking: Vec<Parking>) -> Vec<WindowId> {
        let mut writes = vec![];
        for Parking {
            wid,
            frame,
            corner,
            screen,
            side,
            wsid,
        } in parking
        {
            self.parked.insert(
                wid,
                Parked {
                    before: frame,
                    observed: corner,
                    screen,
                    corner: side,
                    wsid,
                    awaiting: None,
                },
            );
            writes.push((wid, corner));
        }
        self.write_frames_now(&writes, true);
        writes.into_iter().map(|(wid, _)| wid).collect()
    }

    /// Puts parked windows back. A window that the layout places goes to its
    /// frame in the layout now. Any other window, such as a floating one, goes
    /// back to the frame it had before it was parked. The journal entries stay
    /// until the windows report the frames written.
    pub(super) fn unpark_windows(&mut self, wids: &[WindowId]) {
        let released = self.release_parked(wids);
        self.put_back_unplaced(&released);
        self.update_layout(&[], true);
    }

    /// Clears the parked state of the windows and returns each with the frame
    /// it had before it was parked. The next layout pass writes each one's
    /// frame, even if the reactor believes the window is there already.
    pub(super) fn release_parked(&mut self, wids: &[WindowId]) -> Vec<(WindowId, CGRect)> {
        let mut released = vec![];
        let mut interrupted = false;
        for wid in wids {
            let Some(Parked { before: frame, awaiting, .. }) = self.parked.remove(wid) else {
                continue;
            };
            interrupted |= awaiting.is_some();
            self.cancel_parking_echo(*wid);
            self.frame_attempts.remove(wid);
            self.forced_writes.insert(*wid);
            // The user can't be resizing a window in a corner, and
            // `update_layout` doesn't write to the window being resized.
            if self.resizing_window == Some(*wid) {
                self.resizing_window = None;
            }
            released.push((*wid, frame));
        }
        if interrupted {
            self.fail_parking_results_for(
                wids,
                "A context change interrupted parking before its frame was confirmed",
            );
        }
        released
    }

    /// Sends the released windows that the layout doesn't place, such as
    /// floating ones, back to their frames from before parking.
    pub(super) fn put_back_unplaced(&mut self, released: &[(WindowId, CGRect)]) {
        if released.is_empty() {
            return;
        }
        let laid_out = self.windows_in_layout();
        for &(wid, frame) in released {
            if laid_out.contains(&wid) {
                continue;
            }
            let current = self.windows.get(&wid).map_or(frame, |window| window.frame_monotonic);
            let frame = self.on_a_screen(frame, current);
            self.pending_frame_overrides.insert(wid, frame);
        }
    }

    /// `frame`, or, if `frame` is on no screen, `frame` moved onto the screen
    /// that `current` is on, or else onto the main screen. The size stays
    /// where the screen is large enough.
    fn on_a_screen(&self, frame: CGRect, current: CGRect) -> CGRect {
        if self.best_screen_idx_for_window(&frame).is_some() {
            return frame;
        }
        let idx = self.best_screen_idx_for_window(&current).unwrap_or(0);
        let Some(screen) = self.screens.get(idx) else {
            return frame;
        };
        let moved = fit_frame_to_screen(frame, CGSize::new(0.0, 0.0), screen.frame);
        debug!(
            ?frame,
            ?moved,
            "Moving a frame that is on no screen onto a screen"
        );
        moved
    }

    /// The windows that the active layouts of the visible Spaces give a frame.
    /// Floating windows are not among them.
    fn windows_in_layout(&self) -> HashSet<WindowId> {
        self.screens
            .iter()
            .filter_map(|screen| Some((screen.space?, screen.frame)))
            .flat_map(|(space, frame)| self.layout.calculate_layout(space, frame, &self.config))
            .map(|(wid, _)| wid)
            .collect()
    }

    /// Whether a window at `frame` shows 1 square point or less of the screen
    /// it is on.
    fn shows_at_most_a_point(&self, frame: CGRect) -> bool {
        self.best_screen_idx_for_window(&frame)
            .is_none_or(|idx| self.screens[idx].frame.intersection(&frame).area() <= 1.0)
    }

    /// A safe bottom corner on the display that held `frame` before parking.
    fn parking_target(&self, frame: CGRect) -> Option<(CGRect, ScreenId, BottomCorner)> {
        let screen = self.best_screen_idx_for_window(&frame)?;
        let others: Vec<CGRect> = self
            .screens
            .iter()
            .enumerate()
            .filter(|&(idx, _)| idx != screen)
            .map(|(_, other)| other.bounds)
            .collect();
        let own = self.screens[screen];
        let (origin, side) = bounded_bottom_corner(frame.size, own.bounds, &others)?;
        Some((CGRect { origin, size: frame.size }, own.id, side))
    }

    /// Recognizes a surviving parked strip only when the journal can supply
    /// its original frame. A strip alone is never used as the restore frame.
    fn looks_like_parked_strip(&self, frame: CGRect) -> bool {
        self.screens.iter().enumerate().any(|(idx, own)| {
            let others: Vec<CGRect> = self
                .screens
                .iter()
                .enumerate()
                .filter(|&(other, _)| other != idx)
                .map(|(_, screen)| screen.bounds)
                .collect();
            [BottomCorner::Left, BottomCorner::Right]
                .into_iter()
                .any(|side| accepted_bottom_strip(frame, frame.size, own.bounds, &others, side))
        })
    }

    fn accepts_parked_frame(
        &self,
        observed: CGRect,
        requested_size: CGSize,
        screen: ScreenId,
        side: BottomCorner,
    ) -> bool {
        let Some((idx, own)) = self.screens.iter().enumerate().find(|(_, own)| own.id == screen)
        else {
            return false;
        };
        let others: Vec<CGRect> = self
            .screens
            .iter()
            .enumerate()
            .filter(|&(other, _)| other != idx)
            .map(|(_, screen)| screen.bounds)
            .collect();
        accepted_bottom_strip(observed, requested_size, own.bounds, &others, side)
    }

    /// Writes the frames at once, without animation and outside the layout.
    ///
    /// Each write takes a new transaction id and becomes the window's known
    /// frame. It also resets the window's count of repeated writes.
    fn write_frames_now(&mut self, frames: &[(WindowId, CGRect)], confirm_parking: bool) {
        let mut anim = Animation::new();
        for &(wid, frame) in frames {
            let awaiting = self.parked.get(&wid).and_then(|parked| parked.awaiting);
            let Some(window) = self.windows.get_mut(&wid) else {
                continue;
            };
            let Some(app) = self.apps.get(&wid.pid) else { continue };
            self.frame_attempts.remove(&wid);
            self.pending_frame_overrides.remove(&wid);
            let txid = window.next_txid();
            anim.add_window(&app.handle, wid, window.frame_monotonic, frame, false, txid);
            window.frame_monotonic = frame;
            if let Some((previous, _)) = awaiting {
                for pending in &mut self.pending_parking_results {
                    if pending.writes.get(&wid) == Some(&previous) {
                        pending.writes.insert(wid, txid);
                    }
                }
            }
            if let Some(parked) = self.parked.get_mut(&wid)
                && (confirm_parking || awaiting.is_some())
            {
                parked.awaiting =
                    Some((txid, awaiting.map_or_else(Instant::now, |(_, since)| since)));
            }
        }
        self.send_animation(anim, true);
    }

    /// Notes where a parked window is now, from a frame change that comes
    /// after the last write to it.
    pub(super) fn observe_parked(
        &mut self,
        wid: WindowId,
        frame: CGRect,
        last_seen: TransactionId,
    ) {
        let Some(parked) = self.parked.get_mut(&wid) else {
            return;
        };
        if self.windows.get(&wid).is_some_and(|window| window.last_sent_txid == last_seen) {
            parked.observed = frame;
        }
    }

    pub(super) fn pending_parking_writes(&self) -> HashMap<WindowId, TransactionId> {
        self.parked
            .iter()
            .filter_map(|(&wid, parked)| parked.awaiting.map(|(txid, _)| (wid, txid)))
            .collect()
    }

    pub(super) fn new_parking_writes(
        &self,
        before: &HashMap<WindowId, TransactionId>,
    ) -> HashMap<WindowId, TransactionId> {
        self.pending_parking_writes()
            .into_iter()
            .filter(|(wid, txid)| before.get(wid) != Some(txid))
            .collect()
    }

    /// Accepts a parking write only when AX reports a bounded bottom strip.
    /// An ordinary requested echo may carry the frame an app actually allowed,
    /// even after the app thread exhausted its retries.
    pub(super) fn park_write_echoed(
        &mut self,
        wid: WindowId,
        reported: CGRect,
        txid: TransactionId,
    ) -> bool {
        let Some(parked) = self.parked.get(&wid).copied() else {
            return true;
        };
        if parked.awaiting.map(|(awaited, _)| awaited) != Some(txid) {
            return true;
        }
        let window = &self.windows[&wid];
        let target = window.frame_monotonic;
        let identity_matches = window.window_server_id == Some(parked.wsid)
            && self.window_ids.get(&parked.wsid) == Some(&wid)
            && self.apps.get(&wid.pid).is_some_and(|app| {
                self.journal
                    .get(wid.pid, parked.wsid)
                    .is_some_and(|entry| same_app(&entry.bundle_id, &app.info.bundle_id))
            });
        if !identity_matches
            || !self.accepts_parked_frame(reported, target.size, parked.screen, parked.corner)
        {
            let reason = format!(
                "Could not park window {wid:?}: AX reported {reported:?} for {target:?} \
                 without a verified bottom strip and window identity"
            );
            self.abort_failed_parking(reason);
            return false;
        }
        let parked = self.parked.get_mut(&wid).expect("parked window was checked");
        parked.observed = reported;
        parked.awaiting = None;
        self.confirm_parking_result(wid, txid);
        true
    }

    pub(super) fn finish_parking_results(&mut self, error: Option<String>) {
        for pending in std::mem::take(&mut self.pending_parking_results) {
            self.record_command_result(pending.request, error.clone());
        }
    }

    fn confirm_parking_result(&mut self, wid: WindowId, txid: TransactionId) {
        let mut completed = Vec::new();
        for pending in &mut self.pending_parking_results {
            if pending.writes.get(&wid) == Some(&txid) {
                pending.writes.remove(&wid);
            }
            if pending.writes.is_empty() {
                completed.push(pending.request);
            }
        }
        self.pending_parking_results.retain(|pending| !pending.writes.is_empty());
        for request in completed {
            self.record_command_result(request, None);
        }
    }

    fn fail_parking_results_for(&mut self, wids: &[WindowId], reason: &str) {
        let mut failed = Vec::new();
        self.pending_parking_results.retain(|pending| {
            if wids.iter().any(|wid| pending.writes.contains_key(wid)) {
                failed.push(pending.request);
                false
            } else {
                true
            }
        });
        for request in failed {
            self.record_command_result(request, Some(reason.into()));
        }
    }

    pub(super) fn parking_confirmation_timed_out(&self, now: Instant) -> bool {
        self.next_parking_deadline().is_some_and(|deadline| now >= deadline)
    }

    pub(super) fn next_parking_deadline(&self) -> Option<Instant> {
        self.parked
            .values()
            .filter_map(|parked| parked.awaiting.map(|(_, since)| since + PARK_CONFIRM_DEADLINE))
            .min()
    }

    pub(super) fn parking_deadline_tick(&mut self, now: Instant) {
        if self.parking_confirmation_timed_out(now) {
            self.abort_failed_parking("Parking was not confirmed before the deadline".into());
        }
    }

    pub(super) fn abort_failed_parking(&mut self, reason: String) {
        error!("{reason}; showing Everything and restoring parked windows");
        self.finish_parking_results(Some(reason));
        self.switch_guard = Default::default();
        _ = self.contexts.switch_to(crate::model::contexts::ContextKey::Everything);
        self.contexts.set_screen_actives(
            self.screens.iter().map(|screen| screen.id),
            crate::model::contexts::ContextKey::Everything,
        );
        let parked: Vec<WindowId> = self.parked.keys().copied().collect();
        let released = self.release_parked(&parked);
        self.apply_again();
        self.put_back_unplaced(&released);
        self.update_layout(&[], true);
        self.save_contexts();
    }

    /// Handles the echo of a frame write. If the window is back from parking
    /// and the position in the echo is within 16 points of the position
    /// written, the window's journal entry goes.
    ///
    /// The caller has checked that the echo belongs to the last write, whose
    /// target is the window's `frame_monotonic`.
    pub(super) fn confirm_unparked(&mut self, wid: WindowId, reported: CGRect) {
        if self.parked.contains_key(&wid) || self.membership_window(wid) != wid {
            return;
        }
        let Some(window) = self.windows.get(&wid) else { return };
        let Some(wsid) = window.window_server_id else { return };
        if self.journal.get(wid.pid, wsid).is_none() {
            return;
        }
        let target = window.frame_monotonic;
        if !is_back(reported, target) {
            debug!(?wid, ?reported, ?target, "Window is not back from parking yet");
            return;
        }
        for member in self.tabs_of(wid) {
            if let Some(wsid) = self.windows.get(&member).and_then(|window| window.window_server_id)
            {
                self.journal.remove_window(member.pid, wsid);
            }
        }
    }

    /// Forgets a destroyed window's parking state and journal entry.
    pub(super) fn forget_parked_window(&mut self, wid: WindowId, wsid: Option<WindowServerId>) {
        self.fail_parking_results_for(&[wid], "The window closed before parking was confirmed");
        self.parked.remove(&wid);
        if let Some(wsid) = wsid {
            self.journal.remove_window(wid.pid, wsid);
        }
    }

    /// Forgets the parking state and journal entries of an app that is gone.
    pub(super) fn forget_parked_app(&mut self, pid: pid_t) {
        let wids: Vec<WindowId> =
            self.parked.keys().copied().filter(|wid| wid.pid == pid).collect();
        self.fail_parking_results_for(&wids, "The app ended before parking was confirmed");
        self.parked.retain(|wid, _| wid.pid != pid);
        self.journal.remove_app(pid);
    }

    /// Puts the app's windows that the journal listed at startup back at their
    /// frames from before parking. Each entry is put back once, and it stays
    /// in the journal until the window reports the frame.
    pub(super) fn restore_from_journal(&mut self, pid: pid_t) {
        let entries = self.journal.unrestored(pid);
        if entries.is_empty() {
            return;
        }
        let Some(app) = self.apps.get(&pid) else { return };
        let bundle_id = app.info.bundle_id.clone();
        let mut writes = vec![];
        for entry in entries {
            if !same_app(&entry.bundle_id, &bundle_id) {
                info!(
                    pid,
                    journal = ?entry.bundle_id,
                    running = ?bundle_id,
                    "Dropping a journal entry whose pid belongs to another app now"
                );
                self.journal.remove_window(pid, entry.window_server_id);
                continue;
            }
            let Some(&wid) = self.windows.iter().find_map(|(wid, window)| {
                (wid.pid == pid && window.window_server_id == Some(entry.window_server_id))
                    .then_some(wid)
            }) else {
                continue;
            };
            if self.membership_window(wid) != wid {
                continue;
            }
            self.journal.mark_restored(pid, entry.window_server_id);
            let frame = self.on_a_screen(entry.frame.into(), self.windows[&wid].frame_monotonic);
            writes.push((wid, frame));
        }
        if !writes.is_empty() {
            info!(
                pid,
                count = writes.len(),
                "Putting back windows parked before a restart"
            );
            self.write_frames_now(&writes, false);
        }
    }

    /// Drops the journal entries whose process has ended, or whose pid belongs
    /// to another app now.
    pub(super) fn drop_journal_entries_of_ended_apps(&mut self) {
        let lookup = &self.process_lookup;
        self.journal.retain(|entry| {
            let process = lookup(entry.pid);
            let keep = match &process {
                Process::Gone => false,
                Process::Running { bundle_id } => same_app(&entry.bundle_id, bundle_id),
            };
            if !keep {
                info!(
                    pid = entry.pid,
                    journal = ?entry.bundle_id,
                    ?process,
                    "Dropping a journal entry of an app that is not running"
                );
            }
            keep
        });
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Instant, SystemTime};

    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use tempfile::TempDir;
    use test_log::test;
    use tokio::sync::mpsc;

    use super::super::testing::*;
    use super::super::{
        Command, Event, FrameAttempt, MAX_FRAME_ATTEMPTS, Reactor, Requested, TransactionId,
    };
    use crate::actor::app::{Quiet, Request, WindowId, pid_t};
    use crate::actor::layout::{LayoutCommand, LayoutEvent, LayoutManager};
    use crate::actor::parked_journal::{FailingWrites, JournalEntry, ParkedJournal};
    use crate::model::contexts::ContextKey;
    use crate::sys::app::{Process, WindowInfo};
    use crate::sys::event::MouseState;
    use crate::sys::geometry::CGRectExt;
    use crate::sys::screen::{CoordinateConverter, SpaceId};
    use crate::sys::window_server::{WindowServerId, WindowServerInfo, WindowsOnScreen};

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    fn screen() -> CGRect {
        rect(0., 0., 1000., 1000.)
    }

    fn space() -> SpaceId {
        SpaceId::new(1)
    }

    fn wid(idx: u32) -> WindowId {
        WindowId::new(1, idx)
    }

    /// A reactor with one app whose windows are tiled on one screen, and a
    /// journal in a temporary directory.
    struct Setup {
        reactor: Reactor,
        apps: Apps,
        dir: TempDir,
        screen: CGRect,
    }

    impl Setup {
        fn new(windows: usize) -> Setup {
            Setup::new_on(screen(), windows)
        }

        fn new_on(screen: CGRect, windows: usize) -> Setup {
            let mut s = Setup::launching_on(screen, vec![]);
            s.reactor.handle_events(s.apps.make_app(1, make_windows(windows)));
            s.reactor.handle_event(Event::StartupComplete);
            s.apps.simulate_until_quiet(&mut s.reactor);
            s
        }

        /// A reactor on one screen that no app has reached yet, starting with
        /// the journal that an earlier run left with `entries`.
        fn launching(entries: Vec<JournalEntry>) -> Setup {
            Setup::launching_on(screen(), entries)
        }

        fn launching_on(screen: CGRect, entries: Vec<JournalEntry>) -> Setup {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("parked.json");
            if !entries.is_empty() {
                ParkedJournal::open(path, SystemTime::now()).record(entries).unwrap();
            }
            Setup::start(dir, screen)
        }

        /// A reactor on one screen that no app has reached yet, starting with
        /// `contents` in the journal file.
        fn launching_with_file(contents: &[u8]) -> Setup {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("parked.json"), contents).unwrap();
            Setup::start(dir, screen())
        }

        fn start(dir: TempDir, screen: CGRect) -> Setup {
            let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
            reactor.journal =
                ParkedJournal::open(dir.path().join("parked.json"), SystemTime::now());
            reactor.handle_event(Event::ScreenParametersChanged {
                ids: vec![],
                frames: vec![screen],
                bounds: vec![screen],
                spaces: vec![Some(space())],
                scale_factors: vec![1.0],
                converter: CoordinateConverter::default(),
                on_screen: Default::default(),
            });
            Setup {
                reactor,
                apps: Apps::new(),
                dir,
                screen,
            }
        }

        fn journal_path(&self) -> PathBuf {
            self.dir.path().join("parked.json")
        }

        /// The entries in the journal file.
        fn journal_on_disk(&self) -> Vec<JournalEntry> {
            ParkedJournal::open(self.journal_path(), SystemTime::now()).entries().to_vec()
        }

        fn frame(&self, wid: WindowId) -> CGRect {
            self.apps.windows[&wid].frame
        }

        fn tiles(&self) -> Vec<(WindowId, CGRect)> {
            let mut tiles =
                self.reactor.layout.calculate_layout(space(), self.screen, &self.reactor.config);
            tiles.sort_by_key(|(wid, _)| *wid);
            tiles
        }

        /// Asks every app for its windows, which sends each app's visible
        /// windows to the layout again.
        fn refresh_visible_windows(&mut self) {
            self.reactor.update_visible_windows();
            self.apps.simulate_until_quiet(&mut self.reactor);
        }

        /// Closes the window. Its app forgets it, and the reactor learns that
        /// it was destroyed.
        fn close(&mut self, wid: WindowId) {
            self.apps.windows.remove(&wid);
            self.reactor.handle_event(Event::WindowDestroyed(wid));
        }

        fn handle_requests(&mut self, requests: Vec<Request>) {
            for event in self.apps.simulate_events_for_requests(requests) {
                self.reactor.handle_event(event);
            }
        }

        /// Makes `lookup` tell which process has each pid now.
        fn set_processes(&mut self, lookup: impl Fn(pid_t) -> Process + Send + 'static) {
            self.reactor.process_lookup = Box::new(lookup);
        }

        fn parked_windows(&self) -> Vec<WindowId> {
            let mut parked: Vec<WindowId> = self.reactor.parked.keys().copied().collect();
            parked.sort();
            parked
        }
    }

    /// A screen on which three windows side by side get tiles of one size.
    fn wide_screen() -> CGRect {
        rect(0., 0., 1200., 1000.)
    }

    /// The pid, window server id, and frame of each entry.
    fn summary(entries: Vec<JournalEntry>) -> Vec<(i32, u32, CGRect)> {
        entries
            .iter()
            .map(|entry| (entry.pid, entry.window_server_id.as_u32(), entry.frame.into()))
            .collect()
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
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

    #[test]
    fn h1_refuses_a_display_whose_bottom_corners_reach_another_displays_bounds() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        // A display above the main display. Both have a 25-point menu bar.
        let main_visible = rect(0., 25., 1920., 1055.);
        let main_bounds = rect(0., 0., 1920., 1080.);
        let above_visible = rect(0., -1055., 1920., 1055.);
        let above_bounds = rect(0., -1080., 1920., 1080.);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![main_visible, above_visible],
            bounds: vec![main_bounds, above_bounds],
            spaces: vec![Some(space()), None],
            scale_factors: vec![1.0, 1.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        let window = WindowInfo {
            frame: rect(100., -1000., 400., 20.),
            ..make_window(1)
        };
        reactor.handle_events(apps.make_app(1, vec![window]));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        assert!(reactor.park_windows(&[wid(1)]).is_err());
        assert!(apps.requests().is_empty());
        assert!(reactor.parked.is_empty());
    }

    #[test]
    fn r30_the_journal_holds_the_frame_from_before_parking() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(1));

        s.reactor.park_windows(&[wid(1)]).unwrap();

        assert_eq!(
            vec![JournalEntry {
                pid: 1,
                bundle_id: Some("com.testapp1".into()),
                window_server_id: WindowServerId::new(1),
                title: "Window1".into(),
                frame: tile.into(),
            }],
            s.journal_on_disk()
        );
        let parked = rect(999., 999., tile.size.width, tile.size.height);
        assert_eq!(vec![parked], frame_writes(&s.apps.requests(), wid(1)));
    }

    #[test]
    fn r30_a_failed_journal_write_parks_nothing() {
        let mut s = Setup::new(2);
        let txid = s.reactor.windows[&wid(1)].last_sent_txid;
        let frame = s.reactor.windows[&wid(1)].frame_monotonic;

        let failing = FailingWrites::start(s.dir.path());
        let result = s.reactor.park_windows(&[wid(1), wid(2)]);
        drop(failing);

        assert!(result.is_err());
        assert!(s.apps.requests().is_empty());
        assert!(s.reactor.parked.is_empty());
        assert!(s.reactor.journal.entries().is_empty());
        assert_eq!(txid, s.reactor.windows[&wid(1)].last_sent_txid);
        assert_eq!(frame, s.reactor.windows[&wid(1)].frame_monotonic);
        assert!(!s.journal_path().exists());
    }

    #[test]
    fn h2_a_parked_window_keeps_its_tile_and_the_others_keep_their_frames() {
        let mut s = Setup::new(3);
        let tiles = s.tiles();
        let before = [wid(1), wid(3)].map(|wid| s.frame(wid));

        s.reactor.park_windows(&[wid(2)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.refresh_visible_windows();

        assert_eq!(tiles, s.tiles());
        assert_eq!(before, [wid(1), wid(3)].map(|wid| s.frame(wid)));
        let tile = tiles[1].1;
        assert_eq!(
            rect(999., 999., tile.size.width, tile.size.height),
            s.frame(wid(2))
        );
    }

    #[test]
    fn r31_unparking_restores_the_exact_frame_and_the_echo_removes_the_entry() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(1));
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(1, s.journal_on_disk().len(), "the park echo keeps the entry");

        s.reactor.unpark_windows(&[wid(1)]);
        let requests = s.apps.requests();
        assert_eq!(vec![tile], frame_writes(&requests, wid(1)));
        assert_eq!(1, s.journal_on_disk().len(), "the entry waits for the echo");

        for event in s.apps.simulate_events_for_requests(requests) {
            s.reactor.handle_event(event);
        }
        assert_eq!(tile, s.frame(wid(1)));
        assert!(s.journal_on_disk().is_empty());
        assert!(s.reactor.journal.entries().is_empty());
    }

    #[test]
    fn r31_the_entry_stays_while_the_window_is_more_than_16_points_off() {
        let mut s = Setup::new(2);
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);

        s.reactor.unpark_windows(&[wid(1)]);
        let Some(Request::SetWindowFrame(_, target, txid)) =
            s.apps.requests().into_iter().find(|request| {
                matches!(request, Request::SetWindowFrame(request_wid, ..) if *request_wid == wid(1))
            })
        else {
            panic!("no unpark write");
        };
        let echo = |dx: f64| {
            Event::WindowFrameChanged(
                wid(1),
                rect(
                    target.origin.x + dx,
                    target.origin.y,
                    target.size.width,
                    target.size.height,
                ),
                txid,
                Requested(true),
                None,
            )
        };

        s.reactor.handle_event(echo(20.));
        assert_eq!(1, s.journal_on_disk().len());
        s.reactor.handle_event(echo(16.));
        assert!(s.journal_on_disk().is_empty());
    }

    #[test]
    fn r31_the_entry_goes_when_the_window_is_destroyed() {
        let mut s = Setup::new(2);
        s.reactor.park_windows(&[wid(1), wid(2)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);

        s.reactor.handle_event(Event::WindowDestroyed(wid(1)));

        assert_eq!(
            vec![WindowServerId::new(2)],
            s.journal_on_disk()
                .iter()
                .map(|entry| entry.window_server_id)
                .collect::<Vec<_>>()
        );
        assert!(!s.reactor.parked.contains_key(&wid(1)));
    }

    #[test]
    fn r31_the_entries_go_when_the_app_thread_terminates() {
        let mut s = Setup::new(2);
        s.reactor.park_windows(&[wid(1), wid(2)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);

        s.reactor.handle_event(Event::ApplicationThreadTerminated(1));

        assert!(s.journal_on_disk().is_empty());
        assert!(s.reactor.parked.is_empty());
    }

    #[test]
    fn h5_parked_windows_of_one_app_with_one_size_are_not_tabs() {
        let mut s = Setup::new(2);
        let tiles = s.tiles();
        assert_eq!(tiles[0].1.size, tiles[1].1.size);

        s.reactor.park_windows(&[wid(1), wid(2)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(s.frame(wid(1)), s.frame(wid(2)), "both share one corner");
        s.refresh_visible_windows();
        assert_eq!(tiles, s.tiles(), "both keep their tiles");

        s.reactor.handle_event(Event::WindowDestroyed(wid(1)));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(
            vec![wid(2)],
            s.tiles().into_iter().map(|(wid, _)| wid).collect::<Vec<_>>(),
            "the closed window leaves no empty tile"
        );
        assert_eq!(1, s.journal_on_disk().len());
    }

    #[test]
    fn h4_park_and_unpark_writes_take_new_transaction_ids_and_reset_frame_attempts() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(1));
        let gave_up = || FrameAttempt {
            target: tile,
            count: MAX_FRAME_ATTEMPTS + 1,
            last: Instant::now(),
        };
        s.reactor.frame_attempts.insert(wid(1), gave_up());
        let txid = s.reactor.windows[&wid(1)].last_sent_txid;

        s.reactor.park_windows(&[wid(1)]).unwrap();
        let parked = rect(999., 999., tile.size.width, tile.size.height);
        let window = &s.reactor.windows[&wid(1)];
        assert_eq!(txid.0 + 1, window.last_sent_txid.0);
        assert_eq!(parked, window.frame_monotonic);
        assert!(!s.reactor.frame_attempts.contains_key(&wid(1)));
        s.apps.simulate_until_quiet(&mut s.reactor);

        s.reactor.frame_attempts.insert(wid(1), gave_up());
        s.reactor.unpark_windows(&[wid(1)]);
        let window = &s.reactor.windows[&wid(1)];
        assert_eq!(txid.0 + 2, window.last_sent_txid.0);
        assert_eq!(tile, window.frame_monotonic);
        assert_eq!(1, s.reactor.frame_attempts[&wid(1)].count);
        assert_eq!(vec![tile], frame_writes(&s.apps.requests(), wid(1)));
    }

    #[test]
    fn h4_parking_and_unparking_ten_times_quickly_ends_at_the_right_frames() {
        let mut s = Setup::new(2);
        let tiles = [wid(1), wid(2)].map(|wid| s.frame(wid));

        for _ in 0..10 {
            s.reactor.park_windows(&[wid(1), wid(2)]).unwrap();
            s.reactor.unpark_windows(&[wid(1), wid(2)]);
        }
        let requests = s.apps.requests();
        assert_eq!(20, frame_writes(&requests, wid(1)).len());
        assert_eq!(20, frame_writes(&requests, wid(2)).len());
        for event in s.apps.simulate_events_for_requests(requests) {
            s.reactor.handle_event(event);
        }
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(tiles, [wid(1), wid(2)].map(|wid| s.frame(wid)));
        assert!(s.journal_on_disk().is_empty());

        for _ in 0..10 {
            s.reactor.park_windows(&[wid(1), wid(2)]).unwrap();
            s.apps.simulate_until_quiet(&mut s.reactor);
            s.reactor.unpark_windows(&[wid(1), wid(2)]);
            s.apps.simulate_until_quiet(&mut s.reactor);
        }
        assert_eq!(tiles, [wid(1), wid(2)].map(|wid| s.frame(wid)));
        assert!(s.journal_on_disk().is_empty());
        assert!(s.reactor.parked.is_empty());
    }

    fn entry(pid: i32, wsid: u32, frame: CGRect) -> JournalEntry {
        JournalEntry {
            pid,
            bundle_id: Some(format!("com.testapp{pid}")),
            window_server_id: WindowServerId::new(wsid),
            title: format!("Window{wsid}"),
            frame: frame.into(),
        }
    }

    fn window_at(sys_id: u32, frame: CGRect) -> WindowInfo {
        WindowInfo {
            frame,
            sys_id: Some(WindowServerId::new(sys_id)),
            ..make_window(1)
        }
    }

    fn journal_wsids(entries: Vec<JournalEntry>) -> Vec<u32> {
        entries.iter().map(|entry| entry.window_server_id.as_u32()).collect()
    }

    #[test]
    fn a_launch_without_a_journal_writes_none() {
        let s = Setup::new(2);
        assert!(!s.journal_path().exists());
    }

    #[test]
    fn r34_launch_puts_back_each_apps_windows_when_the_app_arrives() {
        let left = rect(0., 0., 500., 1000.);
        let right = rect(500., 0., 500., 1000.);
        let elsewhere = rect(100., 100., 300., 300.);
        let parked = rect(999., 999., 500., 1000.);
        let mut s = Setup::launching(vec![
            entry(1, 11, right),
            entry(1, 12, left),
            entry(2, 21, elsewhere),
            entry(3, 31, left),
        ]);
        // The app with pid 3 quit while Sugarglider was not running.
        s.set_processes(|pid| match pid {
            3 => Process::Gone,
            _ => test_app_process(pid),
        });

        s.reactor
            .handle_events(s.apps.make_app(1, vec![window_at(11, parked), window_at(12, parked)]));
        // The windows go back before the layout sees them, so their tiles keep
        // the order they had before parking and need no second write.
        let requests = s.apps.requests();
        assert_eq!(vec![right], frame_writes(&requests, WindowId::new(1, 1)));
        assert_eq!(vec![left], frame_writes(&requests, WindowId::new(1, 2)));
        for event in s.apps.simulate_events_for_requests(requests) {
            s.reactor.handle_event(event);
        }
        assert_eq!(vec![21, 31], journal_wsids(s.journal_on_disk()));

        s.reactor.handle_events(s.apps.make_app(2, vec![window_at(21, parked)]));
        let requests = s.apps.requests();
        assert_eq!(
            Some(&elsewhere),
            frame_writes(&requests, WindowId::new(2, 1)).first()
        );
        for event in s.apps.simulate_events_for_requests(requests) {
            s.reactor.handle_event(event);
        }
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![31], journal_wsids(s.journal_on_disk()));

        s.reactor.handle_event(Event::StartupComplete);
        assert!(s.journal_on_disk().is_empty());
    }

    #[test]
    fn r34_a_window_the_app_reports_later_goes_back_then() {
        let mut s = Setup::launching(vec![entry(1, 11, screen())]);
        s.reactor.handle_events(s.apps.make_app(1, vec![]));
        s.reactor.handle_event(Event::StartupComplete);
        assert!(s.apps.requests().is_empty());

        let wid = WindowId::new(1, 1);
        s.reactor.handle_event(Event::WindowsDiscovered {
            pid: 1,
            new: vec![(wid, window_at(11, rect(999., 999., 1000., 1000.)))],
            known_visible: vec![wid],
        });
        assert_eq!(vec![screen()], frame_writes(&s.apps.requests(), wid));
    }

    #[test]
    fn r34_each_entry_is_put_back_once() {
        let mut s = Setup::launching(vec![entry(1, 11, screen())]);
        let wid = WindowId::new(1, 1);
        s.reactor
            .handle_events(s.apps.make_app(1, vec![window_at(11, rect(999., 999., 1000., 1000.))]));
        assert_eq!(vec![screen()], frame_writes(&s.apps.requests(), wid));

        // The window hasn't reported the frame, so its entry is still there.
        s.reactor.handle_event(Event::WindowsDiscovered {
            pid: 1,
            new: vec![],
            known_visible: vec![wid],
        });
        assert!(frame_writes(&s.apps.requests(), wid).is_empty());
        assert_eq!(vec![11], journal_wsids(s.journal_on_disk()));
    }

    #[test]
    fn r34_an_entry_whose_pid_another_app_has_now_is_dropped() {
        let elsewhere = rect(100., 100., 300., 300.);
        let mut other_app = entry(1, 11, elsewhere);
        other_app.bundle_id = Some("com.example.other".into());
        let mut s = Setup::launching(vec![other_app]);

        s.reactor
            .handle_events(s.apps.make_app(1, vec![window_at(11, rect(999., 999., 1000., 1000.))]));

        let writes = frame_writes(&s.apps.requests(), WindowId::new(1, 1));
        assert!(!writes.contains(&elsewhere), "{writes:?}");
        assert!(s.journal_on_disk().is_empty());
    }

    #[test]
    fn h1_r35_each_window_keeps_one_point_in_a_corner_of_its_own_display() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        // Two displays side by side. Both have a 25-point menu bar.
        let main_visible = rect(0., 25., 1000., 975.);
        let main_bounds = rect(0., 0., 1000., 1000.);
        let right_visible = rect(1000., 25., 1000., 975.);
        let right_bounds = rect(1000., 0., 1000., 1000.);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![main_visible, right_visible],
            bounds: vec![main_bounds, right_bounds],
            spaces: vec![Some(space()), Some(SpaceId::new(2))],
            scale_factors: vec![1.0, 1.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        let on_right = WindowInfo {
            frame: rect(1200., 100., 50., 50.),
            ..make_window(2)
        };
        reactor.handle_events(apps.make_app(1, vec![make_window(1), on_right]));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);
        assert_eq!(main_visible, apps.windows[&wid(1)].frame);
        assert_eq!(right_visible, apps.windows[&wid(2)].frame);

        reactor.park_windows(&[wid(1), wid(2)]).unwrap();

        // The bottom right corner of the main display would reach into the
        // display on its right.
        let parked_main = rect(-999., 999., 1000., 975.);
        let parked_right = rect(1999., 999., 1000., 975.);
        let requests = apps.requests();
        assert_eq!(vec![parked_main], frame_writes(&requests, wid(1)));
        assert_eq!(vec![parked_right], frame_writes(&requests, wid(2)));
        for (parked, own, other) in [
            (parked_main, main_visible, right_bounds),
            (parked_right, right_visible, main_bounds),
        ] {
            assert_eq!(1.0, parked.intersection(&own).area(), "{parked:?}");
            assert_eq!(0.0, parked.intersection(&other).area(), "{parked:?}");
        }
    }

    #[test]
    fn r30_parking_no_window_writes_no_journal() {
        let mut s = Setup::new(2);

        s.reactor.park_windows(&[]).unwrap();
        s.reactor.park_windows(&[WindowId::new(1, 9), WindowId::new(7, 1)]).unwrap();

        assert!(s.apps.requests().is_empty());
        assert!(file_names(s.dir.path()).is_empty());
    }

    #[test]
    fn r30_parking_a_parked_window_again_writes_nothing_and_keeps_its_entry() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(1));
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let on_disk = fs::read(s.journal_path()).unwrap();
        let txid = s.reactor.windows[&wid(1)].last_sent_txid;

        s.reactor.park_windows(&[wid(1)]).unwrap();

        assert!(s.apps.requests().is_empty());
        assert_eq!(txid, s.reactor.windows[&wid(1)].last_sent_txid);
        assert_eq!(on_disk, fs::read(s.journal_path()).unwrap());
        assert_eq!(vec![entry(1, 1, tile)], s.journal_on_disk());
    }

    #[test]
    fn r30_a_window_listed_twice_in_one_batch_is_parked_once() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(1));

        s.reactor.park_windows(&[wid(1), wid(1)]).unwrap();

        let parked = rect(999., 999., tile.size.width, tile.size.height);
        assert_eq!(vec![parked], frame_writes(&s.apps.requests(), wid(1)));
        assert_eq!(vec![entry(1, 1, tile)], s.journal_on_disk());
    }

    #[test]
    fn r30_a_batch_journals_and_parks_only_the_windows_it_can_park() {
        let mut s = Setup::launching(vec![]);
        let without_window_server_id = WindowInfo { sys_id: None, ..make_window(2) };
        let on_no_screen = WindowInfo {
            frame: rect(5000., 5000., 50., 50.),
            ..make_window(3)
        };
        s.reactor.handle_events(
            s.apps.make_app(1, vec![make_window(1), without_window_server_id, on_no_screen]),
        );
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![(wid(1), screen())], s.tiles());

        s.reactor.park_windows(&[wid(1), wid(2), wid(3)]).unwrap();

        let requests = s.apps.requests();
        assert_eq!(
            vec![rect(999., 999., 1000., 1000.)],
            frame_writes(&requests, wid(1))
        );
        assert!(frame_writes(&requests, wid(2)).is_empty());
        assert!(frame_writes(&requests, wid(3)).is_empty());
        assert_eq!(vec![entry(1, 1, screen())], s.journal_on_disk());
        assert_eq!(vec![wid(1)], s.parked_windows());
    }

    #[test]
    fn r30_a_journal_that_cannot_replace_its_file_parks_none_of_the_batch() {
        let mut s = Setup::new_on(wide_screen(), 3);
        let tiles = [1, 2, 3].map(|idx| s.frame(wid(idx)));
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let state = |s: &Setup| {
            [wid(2), wid(3)].map(|wid| {
                let window = &s.reactor.windows[&wid];
                (window.last_sent_txid, window.frame_monotonic)
            })
        };
        let before = state(&s);
        // A directory now stands where the journal file was.
        fs::remove_file(s.journal_path()).unwrap();
        fs::create_dir(s.journal_path()).unwrap();
        fs::write(s.journal_path().join("in-the-way"), "").unwrap();

        let result = s.reactor.park_windows(&[wid(2), wid(3)]);

        assert!(result.is_err());
        assert!(s.apps.requests().is_empty());
        assert_eq!(before, state(&s));
        assert_eq!(vec![wid(1)], s.parked_windows());
        assert_eq!(&[entry(1, 1, tiles[0])], s.reactor.journal.entries());
        assert_eq!(
            vec!["parked.json"],
            file_names(s.dir.path()),
            "no temporary file stays"
        );
        fs::remove_dir_all(s.journal_path()).unwrap();
        s.reactor.park_windows(&[wid(2)]).unwrap();
        assert_eq!(
            vec![entry(1, 1, tiles[0]), entry(1, 2, tiles[1])],
            s.journal_on_disk()
        );
    }

    #[test]
    fn h4_a_frame_read_from_before_the_park_is_ignored() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(1));
        let tiles = s.tiles();
        let before_park = s.reactor.windows[&wid(1)].last_sent_txid;
        s.reactor.park_windows(&[wid(1)]).unwrap();
        let park = s.apps.requests();

        // The app reports a move it made before it got the park write.
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            rect(20., 20., 500., 1000.),
            before_park,
            Requested(false),
            None,
        ));

        let parked = rect(999., 999., tile.size.width, tile.size.height);
        assert_eq!(parked, s.reactor.windows[&wid(1)].frame_monotonic);
        assert!(s.apps.requests().is_empty());
        assert_eq!(tiles, s.tiles());
        s.handle_requests(park);
        s.reactor.unpark_windows(&[wid(1)]);
        assert_eq!(vec![tile], frame_writes(&s.apps.requests(), wid(1)));
    }

    #[test]
    fn h5_three_windows_of_one_size_when_one_is_parked_and_another_closes() {
        let mut s = Setup::new_on(wide_screen(), 3);
        assert_eq!(
            vec![
                (wid(1), rect(0., 0., 400., 1000.)),
                (wid(2), rect(400., 0., 400., 1000.)),
                (wid(3), rect(800., 0., 400., 1000.)),
            ],
            s.tiles()
        );
        s.reactor.park_windows(&[wid(2)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.refresh_visible_windows();
        let parked = rect(1199., 999., 400., 1000.);
        assert_eq!(parked, s.frame(wid(2)));
        assert_eq!(3, s.tiles().len(), "the parked window keeps its tile");

        s.close(wid(3));

        let left = rect(0., 0., 600., 1000.);
        let right = rect(600., 0., 600., 1000.);
        let requests = s.apps.requests();
        assert_eq!(vec![left], frame_writes(&requests, wid(1)));
        assert!(
            frame_writes(&requests, wid(2)).is_empty(),
            "the parked window stays in its corner"
        );
        s.handle_requests(requests);
        s.refresh_visible_windows();
        assert_eq!(vec![(wid(1), left), (wid(2), right)], s.tiles());
        assert_eq!(left, s.frame(wid(1)));
        assert_eq!(parked, s.frame(wid(2)));
        assert_eq!(vec![wid(2)], s.parked_windows());
        assert_eq!(
            vec![entry(1, 2, rect(400., 0., 400., 1000.))],
            s.journal_on_disk()
        );
    }

    #[test]
    fn h5_three_windows_of_one_size_when_the_parked_one_closes() {
        let mut s = Setup::new_on(wide_screen(), 3);
        s.reactor.park_windows(&[wid(2)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);

        s.close(wid(2));
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.refresh_visible_windows();

        let left = rect(0., 0., 600., 1000.);
        let right = rect(600., 0., 600., 1000.);
        assert_eq!(vec![(wid(1), left), (wid(3), right)], s.tiles());
        assert_eq!([left, right], [wid(1), wid(3)].map(|wid| s.frame(wid)));
        assert!(s.reactor.parked.is_empty());
        assert!(s.journal_on_disk().is_empty());
    }

    #[test]
    fn h3_an_unparked_window_ends_at_its_current_tile() {
        let mut s = Setup::new_on(wide_screen(), 3);
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        // While the window is parked, another window closes and its tile
        // grows.
        s.close(wid(3));
        s.apps.simulate_until_quiet(&mut s.reactor);

        s.reactor.unpark_windows(&[wid(1)]);
        s.apps.simulate_until_quiet(&mut s.reactor);

        let left = rect(0., 0., 600., 1000.);
        let right = rect(600., 0., 600., 1000.);
        assert_eq!(vec![(wid(1), left), (wid(2), right)], s.tiles());
        assert_eq!([left, right], [wid(1), wid(2)].map(|wid| s.frame(wid)));
        assert!(s.journal_on_disk().is_empty());
    }

    #[test]
    fn r31_the_tolerance_is_16_points_on_each_axis_and_the_size_does_not_count() {
        let mut s = Setup::new(2);
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.reactor.unpark_windows(&[wid(1)]);
        assert!(
            s.reactor.parked.is_empty(),
            "the unpark write clears the parked state"
        );
        let Some(Request::SetWindowFrame(_, target, txid)) =
            s.apps.requests().into_iter().find(|request| {
                matches!(request, Request::SetWindowFrame(request_wid, ..) if *request_wid == wid(1))
            })
        else {
            panic!("no unpark write");
        };
        let echo = |dx: f64, dy: f64, dw: f64, dh: f64| {
            Event::WindowFrameChanged(
                wid(1),
                rect(
                    target.origin.x + dx,
                    target.origin.y + dy,
                    target.size.width + dw,
                    target.size.height + dh,
                ),
                txid,
                Requested(true),
                None,
            )
        };

        for off in [
            echo(0., 17., 0., 0.),
            echo(-17., 0., 0., 0.),
            echo(17., -17., 0., 0.),
        ] {
            s.reactor.handle_event(off);
            assert_eq!(1, s.journal_on_disk().len());
        }
        // The app keeps a size of its own.
        s.reactor.handle_event(echo(-16., 16., -200., 300.));
        assert!(s.journal_on_disk().is_empty());
    }

    #[test]
    fn r31_an_unpark_echo_that_arrives_after_the_window_is_parked_again_keeps_the_entry() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(1));
        let parked = rect(999., 999., tile.size.width, tile.size.height);
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);

        s.reactor.unpark_windows(&[wid(1)]);
        let unpark = s.apps.requests();
        assert_eq!(vec![tile], frame_writes(&unpark, wid(1)));
        s.reactor.park_windows(&[wid(1)]).unwrap();
        let park = s.apps.requests();
        assert_eq!(vec![parked], frame_writes(&park, wid(1)));

        // The app answers the unpark write only after the second park.
        s.handle_requests(unpark);
        assert_eq!(vec![wid(1)], s.parked_windows());
        assert_eq!(vec![entry(1, 1, tile)], s.journal_on_disk());
        s.handle_requests(park);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(parked, s.frame(wid(1)));
        assert_eq!(vec![entry(1, 1, tile)], s.journal_on_disk());
    }

    #[test]
    fn r31_only_the_echo_of_the_latest_write_confirms_the_window_is_back() {
        let mut s = Setup::new_on(wide_screen(), 3);
        let tile = s.frame(wid(1));
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);

        s.reactor.unpark_windows(&[wid(1)]);
        let unpark = s.apps.requests();
        assert_eq!(vec![tile], frame_writes(&unpark, wid(1)));
        // Before the app answers, another window closes and the layout writes
        // a wider tile.
        s.close(wid(3));
        let wider = rect(0., 0., 600., 1000.);
        let relayout = s.apps.requests();
        assert_eq!(vec![wider], frame_writes(&relayout, wid(1)));

        s.handle_requests(unpark);
        assert_eq!(
            vec![entry(1, 1, tile)],
            s.journal_on_disk(),
            "the echo of the unpark write is stale"
        );
        s.handle_requests(relayout);
        assert!(s.journal_on_disk().is_empty());
        assert_eq!(wider, s.frame(wid(1)));
    }

    #[test]
    fn r31_the_entry_of_a_window_put_back_after_a_restart_goes_when_it_is_destroyed() {
        let mut s = Setup::launching(vec![entry(1, 11, screen()), entry(2, 21, screen())]);
        let wid = WindowId::new(1, 1);
        s.reactor
            .handle_events(s.apps.make_app(1, vec![window_at(11, rect(999., 999., 1000., 1000.))]));
        assert_eq!(vec![screen()], frame_writes(&s.apps.requests(), wid));

        // The window closes before it reports the frame.
        s.close(wid);

        assert_eq!(vec![(2, 21, screen())], summary(s.journal_on_disk()));
    }

    #[test]
    fn r31_an_app_that_ends_takes_only_its_own_entries() {
        let mut s = Setup::launching_on(wide_screen(), vec![]);
        s.reactor.handle_events(s.apps.make_app(
            1,
            vec![
                window_at(11, rect(100., 100., 50., 50.)),
                window_at(12, rect(300., 100., 50., 50.)),
            ],
        ));
        s.reactor
            .handle_events(s.apps.make_app(2, vec![window_at(21, rect(500., 100., 50., 50.))]));
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        let other_app = WindowId::new(2, 1);
        let all = [WindowId::new(1, 1), WindowId::new(1, 2), other_app];
        let tiles = all.map(|wid| s.frame(wid));
        s.reactor.park_windows(&all).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);

        s.reactor.handle_event(Event::ApplicationTerminated(1));
        _ = s.apps.requests();
        assert_eq!(3, s.journal_on_disk().len(), "the app thread hasn't ended yet");
        s.reactor.handle_event(Event::ApplicationThreadTerminated(1));

        assert_eq!(vec![(2, 21, tiles[2])], summary(s.journal_on_disk()));
        assert_eq!(vec![other_app], s.parked_windows());
    }

    #[test]
    fn r34_entries_of_a_pid_another_app_has_now_are_dropped_and_its_window_is_tiled() {
        let journal_frame = rect(100., 100., 300., 300.);
        let mut entries = vec![entry(1, 11, journal_frame), entry(1, 12, journal_frame)];
        for reused in &mut entries {
            reused.bundle_id = Some("com.example.other".into());
        }
        entries.push(entry(2, 21, journal_frame));
        let mut s = Setup::launching(entries);

        s.reactor
            .handle_events(s.apps.make_app(1, vec![window_at(11, rect(999., 999., 1000., 1000.))]));

        assert_eq!(
            vec![screen()],
            frame_writes(&s.apps.requests(), WindowId::new(1, 1))
        );
        assert_eq!(vec![(2, 21, journal_frame)], summary(s.journal_on_disk()));
    }

    #[test]
    fn r34_a_window_is_put_back_once_however_often_its_app_reports_it() {
        let corner = rect(999., 999., 1000., 1000.);
        let mut s = Setup::launching(vec![entry(1, 11, screen())]);
        let wid = WindowId::new(1, 1);
        let rediscover = |s: &mut Setup| {
            for _ in 0..3 {
                s.reactor.handle_event(Event::WindowsDiscovered {
                    pid: 1,
                    new: vec![],
                    known_visible: vec![wid],
                });
            }
        };

        s.reactor.handle_events(s.apps.make_app(1, vec![window_at(11, corner)]));
        let restore = s.apps.requests();
        assert_eq!(vec![screen()], frame_writes(&restore, wid));
        rediscover(&mut s);
        s.reactor.handle_event(Event::StartupComplete);
        rediscover(&mut s);
        assert!(frame_writes(&s.apps.requests(), wid).is_empty());
        s.handle_requests(restore);
        assert!(s.journal_on_disk().is_empty());

        // Parked again in this session, the window stays parked.
        s.reactor.park_windows(&[wid]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        rediscover(&mut s);
        assert!(frame_writes(&s.apps.requests(), wid).is_empty());
        assert_eq!(corner, s.frame(wid));
        assert_eq!(vec![wid], s.parked_windows());
        assert_eq!(vec![(1, 11, screen())], summary(s.journal_on_disk()));
    }

    #[test]
    fn r34_an_app_whose_thread_registers_after_startup_complete_keeps_its_entries() {
        let elsewhere = rect(100., 100., 300., 300.);
        let mut s = Setup::launching(vec![entry(1, 11, screen()), entry(2, 21, elsewhere)]);
        s.reactor
            .handle_events(s.apps.make_app(1, vec![window_at(11, rect(999., 999., 1000., 1000.))]));
        s.apps.simulate_until_quiet(&mut s.reactor);

        // App 2 is running, but its app thread reaches the reactor only after
        // startup is complete.
        s.reactor.handle_event(Event::StartupComplete);
        assert_eq!(vec![(2, 21, elsewhere)], summary(s.journal_on_disk()));
        s.reactor
            .handle_events(s.apps.make_app(2, vec![window_at(21, rect(999., 999., 300., 300.))]));
        assert_eq!(
            Some(&elsewhere),
            frame_writes(&s.apps.requests(), WindowId::new(2, 1)).first()
        );
    }

    #[test]
    fn r34_an_unreadable_journal_puts_nothing_back_and_a_new_one_starts() {
        let contents: &[u8] = br#"{ "version": 2, "entries": [ { "pid": 1, "bundle_id": "com.testapp1", "window_server_id": 11, "title": "Window1", "frame": { "x": 100, "y": 100, "w": 300, "h": 300 } } ] }"#;
        let mut s = Setup::launching_with_file(contents);
        let names = file_names(s.dir.path());
        assert_eq!(1, names.len(), "{names:?}");
        assert!(
            names[0].starts_with("parked.unreadable-") && names[0].ends_with(".json"),
            "{names:?}"
        );
        assert_eq!(contents, fs::read(s.dir.path().join(&names[0])).unwrap());
        let wid = WindowId::new(1, 1);

        s.reactor
            .handle_events(s.apps.make_app(1, vec![window_at(11, rect(999., 999., 300., 300.))]));
        s.reactor.handle_event(Event::StartupComplete);

        let requests = s.apps.requests();
        assert_eq!(vec![screen()], frame_writes(&requests, wid));
        s.handle_requests(requests);
        s.reactor.park_windows(&[wid]).unwrap();
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(s.journal_path()).unwrap()).unwrap();
        assert_eq!(serde_json::json!(1), written["version"]);
        assert_eq!(vec![(1, 11, screen())], summary(s.journal_on_disk()));
        assert_eq!(
            vec!["parked.json".to_string(), names[0].clone()],
            file_names(s.dir.path())
        );
    }

    #[test]
    fn without_parking_no_journal_file_is_ever_created() {
        let mut s = Setup::launching(vec![]);
        s.reactor.handle_events(s.apps.make_app(1, make_windows(3)));
        s.reactor
            .handle_events(s.apps.make_app(2, vec![window_at(21, rect(700., 100., 50., 50.))]));
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);

        s.close(wid(3));
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.refresh_visible_windows();
        s.reactor.handle_event(Event::ApplicationTerminated(2));
        _ = s.apps.requests();
        s.reactor.handle_event(Event::ApplicationThreadTerminated(2));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert!(file_names(s.dir.path()).is_empty());
        assert!(s.reactor.parked.is_empty());
        assert!(s.reactor.journal.entries().is_empty());
    }

    /// Removes the bundle id from the app that `events` launch.
    fn without_bundle_id(mut events: Vec<Event>) -> Vec<Event> {
        for event in &mut events {
            if let Event::ApplicationLaunched { info, .. } = event {
                info.bundle_id = None;
            }
        }
        events
    }

    #[test]
    fn r34_a_missing_bundle_id_on_one_side_is_another_app() {
        let journal_frame = rect(100., 100., 300., 300.);
        let corner = rect(999., 999., 1000., 1000.);
        for (journal_has_id, app_has_id) in [(false, true), (true, false)] {
            let mut journal_entry = entry(1, 11, journal_frame);
            if !journal_has_id {
                journal_entry.bundle_id = None;
            }
            let mut s = Setup::launching(vec![journal_entry, entry(2, 21, journal_frame)]);
            let launch = s.apps.make_app(1, vec![window_at(11, corner)]);
            let launch = if app_has_id {
                launch
            } else {
                without_bundle_id(launch)
            };

            s.reactor.handle_events(launch);

            let case = (journal_has_id, app_has_id);
            assert_eq!(
                vec![screen()],
                frame_writes(&s.apps.requests(), WindowId::new(1, 1)),
                "{case:?}"
            );
            assert_eq!(
                vec![(2, 21, journal_frame)],
                summary(s.journal_on_disk()),
                "{case:?}"
            );
        }
    }

    #[test]
    fn r34_an_app_without_a_bundle_id_gets_its_windows_back() {
        let journal_frame = rect(100., 100., 300., 300.);
        let mut journal_entry = entry(1, 11, journal_frame);
        journal_entry.bundle_id = None;
        let mut s = Setup::launching(vec![journal_entry.clone(), entry(2, 21, journal_frame)]);
        s.set_processes(|pid| match pid {
            1 => Process::Running { bundle_id: None },
            _ => test_app_process(pid),
        });
        s.reactor.handle_event(Event::StartupComplete);
        assert_eq!(
            vec![(1, 11, journal_frame), (2, 21, journal_frame)],
            summary(s.journal_on_disk())
        );
        let launch = s.apps.make_app(1, vec![window_at(11, rect(999., 999., 300., 300.))]);

        s.reactor.handle_events(without_bundle_id(launch));

        let wid = WindowId::new(1, 1);
        let requests = s.apps.requests();
        assert_eq!(Some(&journal_frame), frame_writes(&requests, wid).first());
        s.handle_requests(requests);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![(2, 21, journal_frame)], summary(s.journal_on_disk()));
    }

    #[test]
    fn r34_an_app_that_registers_after_startup_complete_is_put_back() {
        let elsewhere = rect(100., 100., 300., 300.);
        let mut s = Setup::launching(vec![entry(1, 11, elsewhere)]);

        s.reactor.handle_event(Event::StartupComplete);
        assert_eq!(vec![(1, 11, elsewhere)], summary(s.journal_on_disk()));
        s.reactor
            .handle_events(s.apps.make_app(1, vec![window_at(11, rect(999., 999., 1000., 1000.))]));

        let requests = s.apps.requests();
        let writes = frame_writes(&requests, WindowId::new(1, 1));
        assert_eq!(Some(&elsewhere), writes.first(), "{writes:?}");
        assert_eq!(vec![(1, 11, elsewhere)], summary(s.journal_on_disk()));
        s.handle_requests(requests);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert!(s.journal_on_disk().is_empty());
    }

    #[test]
    fn r34_the_entries_of_an_app_that_never_registers_stay_while_it_runs() {
        let elsewhere = rect(100., 100., 300., 300.);
        let mut s = Setup::launching(vec![entry(1, 11, screen()), entry(2, 21, elsewhere)]);
        s.reactor
            .handle_events(s.apps.make_app(1, vec![window_at(11, rect(999., 999., 1000., 1000.))]));
        s.apps.simulate_until_quiet(&mut s.reactor);

        // App 2 is running, but its app thread never reaches the reactor.
        s.reactor.handle_event(Event::StartupComplete);
        s.refresh_visible_windows();

        assert_eq!(vec![(2, 21, elsewhere)], summary(s.journal_on_disk()));
    }

    #[test]
    fn r34_startup_complete_drops_entries_by_the_process_that_has_their_pid() {
        let frame = rect(100., 100., 300., 300.);
        let mut s = Setup::launching(vec![
            entry(1, 11, frame),
            entry(2, 21, frame),
            entry(3, 31, frame),
            entry(4, 41, frame),
            entry(5, 51, frame),
        ]);
        s.set_processes(|pid| match pid {
            // Registered below, but its process has ended since.
            1 => Process::Gone,
            // Running, but never registered.
            2 => test_app_process(pid),
            3 => Process::Gone,
            4 => Process::Running {
                bundle_id: Some("com.example.other".into()),
            },
            _ => Process::Running { bundle_id: None },
        });
        s.reactor
            .handle_events(s.apps.make_app(1, vec![window_at(11, rect(999., 999., 300., 300.))]));

        s.reactor.handle_event(Event::StartupComplete);

        assert_eq!(vec![(2, 21, frame)], summary(s.journal_on_disk()));
    }

    #[test]
    fn r30_parking_returns_only_the_windows_it_parked() {
        let mut s = Setup::launching(vec![]);
        let without_window_server_id = WindowInfo { sys_id: None, ..make_window(2) };
        let on_no_screen = WindowInfo {
            frame: rect(5000., 5000., 50., 50.),
            ..make_window(3)
        };
        s.reactor.handle_events(s.apps.make_app(
            1,
            vec![
                make_window(1),
                without_window_server_id,
                on_no_screen,
                make_window(4),
            ],
        ));
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![wid(1)], s.reactor.park_windows(&[wid(1)]).unwrap());
        s.apps.simulate_until_quiet(&mut s.reactor);

        let batch = [wid(1), wid(2), wid(3), wid(4), wid(4), WindowId::new(7, 1)];
        let parked = s.reactor.park_windows(&batch);

        assert_eq!(vec![wid(4)], parked.unwrap());
        assert_eq!(vec![wid(1), wid(4)], s.parked_windows());
        assert!(s.reactor.park_windows(&[]).unwrap().is_empty());
    }

    #[test]
    fn h3_unparking_after_the_layout_changed_writes_only_the_new_tile() {
        let mut s = Setup::new(3);
        let old_tile = s.frame(wid(1));
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.reactor.handle_event(Event::WindowDestroyed(wid(3)));
        s.apps.simulate_until_quiet(&mut s.reactor);
        let new_tile = s.tiles().into_iter().find(|(window, _)| *window == wid(1)).unwrap().1;
        assert_ne!(old_tile, new_tile);

        s.reactor.unpark_windows(&[wid(1)]);

        let requests = s.apps.requests();
        assert_eq!(vec![new_tile], frame_writes(&requests, wid(1)));
        assert_eq!(1, s.journal_on_disk().len());
        s.handle_requests(requests);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(new_tile, s.frame(wid(1)));
        assert!(s.journal_on_disk().is_empty());
    }

    #[test]
    fn h3_a_floating_window_goes_back_to_its_frame_from_before_parking() {
        let mut s = Setup::new(2);
        s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
        s.reactor.send_layout_event(LayoutEvent::WindowFocused(vec![space()], wid(1)));
        s.reactor.handle_event(Event::Command(Command::Layout(
            LayoutCommand::ToggleWindowFloating,
        )));
        s.apps.simulate_until_quiet(&mut s.reactor);
        let floating = rect(100., 100., 50., 50.);
        assert_eq!(floating, s.frame(wid(1)));
        assert_eq!(vec![(wid(2), screen())], s.tiles());
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(rect(999., 999., 50., 50.), s.frame(wid(1)));

        s.reactor.unpark_windows(&[wid(1)]);

        let requests = s.apps.requests();
        assert_eq!(vec![floating], frame_writes(&requests, wid(1)));
        s.handle_requests(requests);
        assert_eq!(floating, s.frame(wid(1)));
        assert!(s.journal_on_disk().is_empty());
    }

    #[test]
    fn h3_an_unpark_puts_back_a_window_left_marked_as_being_resized() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(1));
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        // A mouse-up was missed, so the window still counts as being resized.
        s.reactor.resizing_window = Some(wid(1));

        s.reactor.unpark_windows(&[wid(1)]);
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(tile, s.frame(wid(1)));
        s.reactor.park_windows(&[wid(1)]).unwrap();
        assert_eq!(vec![entry(1, 1, tile)], s.journal_on_disk());
    }

    #[test]
    fn r30_a_window_still_in_its_corner_keeps_its_entry_when_parked_again() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(1));
        let corner = rect(999., 999., tile.size.width, tile.size.height);
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        // The window is no longer parked, but no write has moved it out of
        // its corner.
        s.reactor.parked.remove(&wid(1));
        assert_eq!(corner, s.reactor.windows[&wid(1)].frame_monotonic);

        assert_eq!(vec![wid(1)], s.reactor.park_windows(&[wid(1)]).unwrap());

        assert_eq!(vec![entry(1, 1, tile)], s.journal_on_disk());
        assert_eq!(
            Some(tile),
            s.reactor.parked.get(&wid(1)).map(|parked| parked.before)
        );
    }

    #[test]
    fn r30_a_window_moved_after_it_was_put_back_is_journaled_where_it_is() {
        let mut s = Setup::new(2);
        s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
        s.reactor.send_layout_event(LayoutEvent::WindowFocused(vec![space()], wid(1)));
        s.reactor.handle_event(Event::Command(Command::Layout(
            LayoutCommand::ToggleWindowFloating,
        )));
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.reactor.unpark_windows(&[wid(1)]);
        _ = s.apps.requests();
        assert_eq!(1, s.journal_on_disk().len(), "no echo has confirmed the unpark");

        // The user moves the floating window.
        let moved = rect(300., 400., 50., 50.);
        let txid = s.reactor.windows[&wid(1)].last_sent_txid;
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            moved,
            txid,
            Requested(false),
            None,
        ));
        s.reactor.park_windows(&[wid(1)]).unwrap();

        assert_eq!(vec![entry(1, 1, moved)], s.journal_on_disk());
    }

    #[test]
    fn r30_a_window_in_a_corner_without_an_entry_is_not_parked() {
        let mut s = Setup::new(2);
        s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
        s.reactor.send_layout_event(LayoutEvent::WindowFocused(vec![space()], wid(1)));
        s.reactor.handle_event(Event::Command(Command::Layout(
            LayoutCommand::ToggleWindowFloating,
        )));
        s.apps.simulate_until_quiet(&mut s.reactor);
        // The floating window is moved into a corner, as parking would.
        let corner = rect(999., 999., 50., 50.);
        let txid = s.reactor.windows[&wid(1)].last_sent_txid;
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            corner,
            txid,
            Requested(false),
            None,
        ));
        assert!(s.apps.requests().is_empty());

        assert!(s.reactor.park_windows(&[wid(1)]).unwrap().is_empty());

        assert!(s.apps.requests().is_empty());
        assert!(s.reactor.parked.is_empty());
        assert!(file_names(s.dir.path()).is_empty());
    }

    /// Makes window 1 of `s` float at the frame it had before it was tiled,
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

    /// Reports that the app moved or resized the window to `frame` on its own.
    fn app_moves(s: &mut Setup, wid: WindowId, frame: CGRect, mouse: Option<MouseState>) {
        let txid = s.reactor.windows[&wid].last_sent_txid;
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid,
            frame,
            txid,
            Requested(false),
            mouse,
        ));
        s.apps.windows.get_mut(&wid).unwrap().frame = frame;
    }

    #[test]
    fn h4_an_unpark_writes_the_frame_when_the_known_frame_already_matches_it() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(1));
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        // Something set the frame the reactor knows back to the tile, while
        // the window is still in its corner.
        s.reactor.windows.get_mut(&wid(1)).unwrap().frame_monotonic = tile;

        s.reactor.unpark_windows(&[wid(1)]);

        let requests = s.apps.requests();
        assert_eq!(vec![tile], frame_writes(&requests, wid(1)));
        s.handle_requests(requests);
        assert_eq!(tile, s.frame(wid(1)));
        assert!(s.journal_on_disk().is_empty());
        assert!(s.reactor.forced_writes.is_empty());
        s.refresh_visible_windows();
        assert!(
            frame_writes(&s.apps.requests(), wid(1)).is_empty(),
            "only the unpark write is forced"
        );
    }

    #[test]
    fn h2_a_parked_window_that_its_app_resizes_changes_no_tile() {
        let mut s = Setup::new(3);
        let tiles = s.tiles();
        let others = [wid(1), wid(3)].map(|wid| s.frame(wid));
        s.reactor.park_windows(&[wid(2)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let corner = s.reactor.windows[&wid(2)].frame_monotonic;

        let wider = rect(corner.origin.x, corner.origin.y, 500., 1000.);
        app_moves(&mut s, wid(2), wider, None);
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(tiles, s.tiles());
        assert_eq!(others, [wid(1), wid(3)].map(|wid| s.frame(wid)));
        assert_eq!(corner, s.reactor.windows[&wid(2)].frame_monotonic);
    }

    #[test]
    fn h2_a_parked_window_that_its_app_moves_changes_no_layout_state() {
        let mut s = Setup::new(3);
        let tiles = s.tiles();
        s.reactor.park_windows(&[wid(2)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);

        // Dragged with the mouse, moved to another size, then moved off
        // every screen.
        app_moves(
            &mut s,
            wid(2),
            rect(10., 10., 200., 200.),
            Some(MouseState::Down),
        );
        app_moves(&mut s, wid(2), rect(5000., 10., 200., 200.), None);

        assert!(!s.reactor.in_drag);
        assert_eq!(None, s.reactor.resizing_window);
        assert!(s.apps.requests().is_empty());
        s.refresh_visible_windows();
        assert_eq!(tiles, s.tiles());
    }

    #[test]
    fn h4_a_parked_window_that_its_app_moves_back_is_still_put_back() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(1));
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        app_moves(&mut s, wid(1), tile, None);

        s.reactor.unpark_windows(&[wid(1)]);

        let requests = s.apps.requests();
        assert_eq!(vec![tile], frame_writes(&requests, wid(1)));
        // An app reports the frame after every write, even one that doesn't
        // move the window. The test apps report only a change.
        let txid = s.reactor.windows[&wid(1)].last_sent_txid;
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            tile,
            txid,
            Requested(true),
            None,
        ));
        assert!(s.journal_on_disk().is_empty());
    }

    #[test]
    fn h2_a_parked_floating_window_that_its_app_moves_keeps_its_restore_frame() {
        let mut s = Setup::new(2);
        float_window_1(&mut s);
        let floating = rect(100., 100., 50., 50.);
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);

        app_moves(&mut s, wid(1), rect(500., 500., 50., 50.), None);

        assert_eq!(Some(floating), s.reactor.layout.floating_restore_frame(wid(1)));
        s.reactor.unpark_windows(&[wid(1)]);
        assert_eq!(vec![floating], frame_writes(&s.apps.requests(), wid(1)));
    }

    #[test]
    fn h2_a_parked_window_keeps_the_frames_it_reports_apart_from_the_layout() {
        let mut s = Setup::new(2);
        let tiles = s.tiles();
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let corner = s.frame(wid(1));
        let observed = |s: &Setup| s.reactor.parked[&wid(1)].observed;
        assert_eq!(corner, observed(&s));

        let moved = rect(20., 30., 500., 1000.);
        app_moves(&mut s, wid(1), moved, None);
        assert_eq!(moved, observed(&s));

        // A report from before the last write is stale.
        let txid = s.reactor.windows[&wid(1)].last_sent_txid;
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            rect(40., 50., 500., 1000.),
            TransactionId(txid.0 - 1),
            Requested(false),
            None,
        ));
        assert_eq!(moved, observed(&s));

        let snapshot = rect(60., 70., 500., 1000.);
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(vec![
                WindowServerInfo {
                    id: WindowServerId::new(1),
                    pid: 1,
                    layer: 0,
                    frame: snapshot,
                },
                WindowServerInfo {
                    id: WindowServerId::new(2),
                    pid: 1,
                    layer: 0,
                    frame: tiles[1].1,
                },
            ]),
        });
        assert_eq!(snapshot, observed(&s));

        assert_eq!(corner, s.reactor.windows[&wid(1)].frame_monotonic);
        s.refresh_visible_windows();
        assert_eq!(tiles, s.tiles());
        assert!(s.apps.requests().is_empty());
    }

    #[test]
    fn h2_the_mouse_over_a_parked_window_does_not_focus_it() {
        let mut s = Setup::new(3);
        s.reactor.handle_event(Event::ApplicationActivated(1, Quiet::No));
        s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
        assert_eq!(Some(wid(1)), s.reactor.main_window());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        s.reactor.raise_manager_tx = raise_manager_tx;
        let over = |idx| Event::MouseMovedOverWindow(WindowServerId::new(idx), None);
        s.reactor.handle_event(over(2));
        assert!(raise_manager_rx.try_recv().is_ok(), "a window that isn't parked");
        s.reactor.park_windows(&[wid(3)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);

        s.reactor.handle_event(over(3));

        assert!(raise_manager_rx.try_recv().is_err());
    }

    #[test]
    fn h4_a_window_server_snapshot_leaves_a_parked_window_in_its_corner() {
        let mut s = Setup::new(2);
        let tiles = [wid(1), wid(2)].map(|wid| s.frame(wid));
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let corner = s.reactor.windows[&wid(1)].frame_monotonic;

        // The snapshot was taken before the park write landed.
        let snapshot = [1, 2]
            .into_iter()
            .zip(tiles)
            .map(|(wsid, frame)| WindowServerInfo {
                id: WindowServerId::new(wsid),
                pid: 1,
                layer: 0,
                frame,
            })
            .collect();
        s.reactor.handle_event(Event::SpaceChanged(
            vec![Some(space())],
            WindowsOnScreen::new(snapshot),
        ));
        s.apps.simulate_until_quiet(&mut s.reactor);

        assert_eq!(corner, s.reactor.windows[&wid(1)].frame_monotonic);
        s.reactor.unpark_windows(&[wid(1)]);
        assert_eq!(vec![tiles[0]], frame_writes(&s.apps.requests(), wid(1)));
    }

    #[test]
    fn r34_a_window_parked_on_a_display_that_is_gone_goes_back_onto_a_screen() {
        let on_the_gone_display = rect(1100., 100., 400., 400.);
        let mut s = Setup::launching(vec![entry(1, 11, on_the_gone_display)]);

        s.reactor
            .handle_events(s.apps.make_app(1, vec![window_at(11, rect(999., 999., 400., 400.))]));

        let wid = WindowId::new(1, 1);
        let writes = frame_writes(&s.apps.requests(), wid);
        assert_eq!(Some(&rect(600., 100., 400., 400.)), writes.first(), "{writes:?}");
        assert_eq!(vec![(wid, screen())], s.tiles());
    }

    /// The displays are now at `frames`, without menu bars. Display n shows
    /// Space n.
    fn displays(frames: Vec<CGRect>) -> Event {
        Event::ScreenParametersChanged {
            ids: vec![],
            bounds: frames.clone(),
            spaces: (1..=frames.len() as u64).map(|id| Some(SpaceId::new(id))).collect(),
            scale_factors: vec![1.0; frames.len()],
            frames,
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        }
    }

    #[test]
    fn h3_a_floating_window_whose_display_is_gone_goes_back_onto_a_screen() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let main = rect(0., 0., 1000., 1000.);
        let right = rect(1000., 0., 1000., 1000.);
        reactor.handle_event(displays(vec![main, right]));
        let floating = rect(1200., 100., 300., 300.);
        let window = WindowInfo {
            frame: floating,
            is_resizable: false,
            ..make_window(1)
        };
        reactor.handle_events(apps.make_app(1, vec![window]));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);
        assert_eq!(floating, apps.windows[&wid(1)].frame);
        reactor.park_windows(&[wid(1)]).unwrap();
        apps.simulate_until_quiet(&mut reactor);

        // The display on the right is unplugged.
        reactor.handle_event(displays(vec![main]));
        apps.simulate_until_quiet(&mut reactor);
        reactor.unpark_windows(&[wid(1)]);

        assert_eq!(
            vec![rect(700., 100., 300., 300.)],
            frame_writes(&apps.requests(), wid(1))
        );
    }

    /// Three displays side by side, each with a 25-point menu bar, and app 1
    /// with window 1 on the middle display and window 2 on the right one.
    /// Window 1 is the main window.
    fn three_displays() -> (Reactor, Apps) {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let middle = rect(0., 0., 1000., 1000.);
        let left = rect(-1000., 0., 1000., 1000.);
        let right = rect(1000., 0., 1000., 1000.);
        let visible = |bounds: CGRect| rect(bounds.origin.x, 25., 1000., 975.);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![visible(middle), visible(left), visible(right)],
            bounds: vec![middle, left, right],
            spaces: vec![
                Some(SpaceId::new(1)),
                Some(SpaceId::new(2)),
                Some(SpaceId::new(3)),
            ],
            scale_factors: vec![1.0; 3],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        let on_middle = WindowInfo {
            frame: rect(100., 100., 200., 200.),
            ..make_window(1)
        };
        let on_right = WindowInfo {
            frame: rect(1200., 100., 200., 200.),
            ..make_window(2)
        };
        reactor.handle_events(apps.make_app_with_opts(
            1,
            vec![on_middle, on_right],
            Some(wid(1)),
            true,
        ));
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);
        (reactor, apps)
    }

    #[test]
    fn h1_a_middle_display_without_a_clear_bottom_corner_is_not_parked() {
        let (mut reactor, mut apps) = three_displays();
        let middle = rect(0., 25., 1000., 975.);
        let right = rect(1000., 25., 1000., 975.);
        let tiles = |reactor: &Reactor| {
            [(SpaceId::new(1), middle), (SpaceId::new(3), right)].map(|(space, screen)| {
                reactor.layout.calculate_layout(space, screen, &reactor.config)
            })
        };
        assert_eq!([vec![(wid(1), middle)], vec![(wid(2), right)]], tiles(&reactor));

        assert!(reactor.park_windows(&[wid(1)]).is_err());
        assert!(apps.requests().is_empty());
        assert!(reactor.parked.is_empty());
        assert_eq!([vec![(wid(1), middle)], vec![(wid(2), right)]], tiles(&reactor));
    }

    #[test]
    fn h2_a_parked_window_reaches_the_layout_with_its_frame_from_before_parking() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(1));
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        // The layout loses the window and everything it knew about it.
        s.reactor.send_layout_event(LayoutEvent::WindowRemoved(wid(1)));
        assert_eq!(None, s.reactor.layout.floating_restore_frame(wid(1)));

        s.refresh_visible_windows();

        assert_eq!(Some(tile), s.reactor.layout.floating_restore_frame(wid(1)));
        assert_eq!(
            vec![wid(1), wid(2)],
            s.tiles().into_iter().map(|(wid, _)| wid).collect::<Vec<_>>()
        );
        assert_eq!(rect(999., 999., 500., 1000.), s.frame(wid(1)));
    }

    /// Two displays side by side, a journal in a temporary directory, and app
    /// 1 with window 1 tiled on the right display and window 2 on the main
    /// one. Window 1 is parked in the bottom right corner of the right
    /// display.
    fn parked_on_the_right_display() -> (Reactor, Apps, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.journal = ParkedJournal::open(dir.path().join("parked.json"), SystemTime::now());
        let main = rect(0., 0., 1000., 1000.);
        let right = rect(1000., 0., 1000., 1000.);
        reactor.handle_event(displays(vec![main, right]));
        let on_right = WindowInfo {
            frame: rect(1200., 100., 200., 200.),
            ..make_window(1)
        };
        reactor.handle_events(apps.make_app(1, vec![on_right, make_window(2)]));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);
        assert_eq!(right, apps.windows[&wid(1)].frame);
        reactor.park_windows(&[wid(1)]).unwrap();
        apps.simulate_until_quiet(&mut reactor);
        assert_eq!(rect(1999., 999., 1000., 1000.), apps.windows[&wid(1)].frame);
        (reactor, apps, dir)
    }

    fn journal_in(dir: &TempDir) -> Vec<JournalEntry> {
        ParkedJournal::open(dir.path().join("parked.json"), SystemTime::now())
            .entries()
            .to_vec()
    }

    #[test]
    fn h1_a_window_parked_on_a_display_that_is_unplugged_is_parked_on_another() {
        let (mut reactor, mut apps, dir) = parked_on_the_right_display();
        let main = rect(0., 0., 1000., 1000.);
        // The right display is unplugged, and macOS moves the window onto the
        // main display.
        let txid = reactor.windows[&wid(1)].last_sent_txid;
        reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            main,
            txid,
            Requested(false),
            None,
        ));
        apps.windows.get_mut(&wid(1)).unwrap().frame = main;

        reactor.handle_event(displays(vec![main]));

        let corner = rect(999., 999., 1000., 1000.);
        let requests = apps.requests();
        assert_eq!(vec![corner], frame_writes(&requests, wid(1)));
        assert_eq!(vec![entry(1, 1, main)], journal_in(&dir));
        for event in apps.simulate_events_for_requests(requests) {
            reactor.handle_event(event);
        }
        apps.simulate_until_quiet(&mut reactor);
        assert_eq!(corner, apps.windows[&wid(1)].frame);
        assert_eq!(vec![entry(1, 1, main)], journal_in(&dir));

        reactor.unpark_windows(&[wid(1)]);
        assert_eq!(vec![main], frame_writes(&apps.requests(), wid(1)));
    }

    #[test]
    fn h1_r30_a_window_is_not_parked_again_before_its_new_journal_entry_is_written() {
        let (mut reactor, mut apps, dir) = parked_on_the_right_display();
        let right = rect(1000., 0., 1000., 1000.);

        let failing = FailingWrites::start(dir.path());
        reactor.handle_event(displays(vec![rect(0., 0., 1000., 1000.)]));
        drop(failing);

        assert!(!frame_writes(&apps.requests(), wid(1)).is_empty());
        assert!(reactor.parked.is_empty());
        assert_eq!(ContextKey::Everything, reactor.contexts.active());
        assert_eq!(vec![entry(1, 1, right)], journal_in(&dir));
    }

    #[test]
    fn repark_preflights_the_whole_batch_before_changing_any_journal_entry() {
        let mut s = Setup::new(3);
        s.reactor.park_windows(&[wid(2), wid(3)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let original = s.journal_on_disk();
        assert_eq!(2, original.len());

        let shifted = rect(1000., 0., 1000., 1000.);
        s.reactor.screens[0].frame = shifted;
        s.reactor.screens[0].bounds = shifted;
        s.reactor.repark_counts.insert(wid(3), super::MAX_REPARKS);
        let error = s.reactor.repark_moved_windows().unwrap_err();

        assert!(error.contains("retry limit"));
        assert_eq!(original, s.journal_on_disk());
        assert!(s.reactor.parked.is_empty());
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert!(
            frame_writes(&s.apps.requests(), wid(2))
                .iter()
                .all(|frame| frame.origin.x != 1999.)
        );
    }

    #[test]
    fn h1_a_display_added_next_to_a_parked_windows_corner_moves_it_to_a_clear_one() {
        let mut s = Setup::new(2);
        let tile = s.frame(wid(2));
        s.reactor.park_windows(&[wid(2)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(rect(999., 999., 500., 1000.), s.frame(wid(2)));

        // The same displays again move nothing.
        s.reactor.handle_event(displays(vec![screen()]));
        assert!(frame_writes(&s.apps.requests(), wid(2)).is_empty());

        // A display on the right would take part of the corner.
        s.reactor.handle_event(displays(vec![screen(), rect(1000., 0., 1000., 1000.)]));

        let requests = s.apps.requests();
        assert_eq!(
            vec![rect(-499., 999., 500., 1000.)],
            frame_writes(&requests, wid(2))
        );
        assert_eq!(vec![entry(1, 2, tile)], s.journal_on_disk());
        s.handle_requests(requests);
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.reactor.unpark_windows(&[wid(2)]);
        assert_eq!(vec![tile], frame_writes(&s.apps.requests(), wid(2)));
    }

    #[test]
    fn r31_the_next_event_writes_a_removal_that_could_not_be_written() {
        let mut s = Setup::new(2);
        let tiles = [wid(1), wid(2)].map(|wid| s.frame(wid));
        s.reactor.park_windows(&[wid(1), wid(2)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let failing = FailingWrites::start(s.dir.path());
        s.close(wid(1));
        drop(failing);
        assert_eq!(
            vec![entry(1, 1, tiles[0]), entry(1, 2, tiles[1])],
            s.journal_on_disk()
        );

        s.reactor.handle_event(Event::MouseUp);

        assert_eq!(vec![entry(1, 2, tiles[1])], s.journal_on_disk());
    }

    #[test]
    fn h5_a_window_that_shows_up_at_a_parked_windows_corner_is_not_its_tab() {
        let mut s = Setup::new(2);
        s.reactor.park_windows(&[wid(1)]).unwrap();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let corner = s.frame(wid(1));

        // A new window of the same app appears at exactly the parked frame.
        let new = WindowInfo {
            frame: corner,
            ..make_window(3)
        };
        s.apps.windows.insert(
            wid(3),
            WindowState {
                frame: corner,
                ..Default::default()
            },
        );
        s.reactor.handle_event(Event::WindowCreated(wid(3), new, MouseState::Up));
        s.reactor.handle_event(Event::WindowBecameVisible(wid(3)));
        s.apps.simulate_until_quiet(&mut s.reactor);

        let tiled: Vec<WindowId> = s.tiles().into_iter().map(|(wid, _)| wid).collect();
        assert_eq!(vec![wid(1), wid(2), wid(3)], tiled);
        assert_eq!(corner, s.frame(wid(1)));
    }
}

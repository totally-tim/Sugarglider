// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reactor tests for the focus rules of M5b, taken from the spec: focus from
//! outside on windows in no context, pinned windows, members, new windows and
//! panels (R24), the end of a switch's wait (R25), raising a member instead
//! of switching (R40), a new tab that takes focus (R36), and activating
//! Finder when no window can take focus (R12 step 6). The raise manager's
//! channel is replaced by one the test reads, and the test sends the
//! activation and main window events that the raises would cause.

use test_log::test;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::Span;

use super::*;
use crate::actor::reactor::testing::{WindowState, test_app_info};
use crate::sys::app::AppInfo;

type Raises = UnboundedReceiver<(Span, raise::Event)>;

/// Replaces the raise manager's channel with one the test reads.
fn capture_raises(s: &mut Setup) -> Raises {
    let (raise_manager_tx, raise_manager_rx) = mpsc::unbounded_channel();
    s.reactor.raise_manager_tx = raise_manager_tx;
    raise_manager_rx
}

/// The sequence id and the focus window of each raise request so far.
fn raise_requests(raises: &mut Raises) -> Vec<(u64, Option<WindowId>)> {
    let mut requests = vec![];
    while let Ok((_, event)) = raises.try_recv() {
        if let raise::Event::RaiseRequest(request) = event {
            requests.push((request.sequence_id, request.focus_window.map(|(wid, _)| wid)));
        }
    }
    requests
}

/// The focus window of each raise request so far.
fn focused(raises: &mut Raises) -> Vec<Option<WindowId>> {
    raise_requests(raises).into_iter().map(|(_, focus)| focus).collect()
}

/// Ends the raise sequences of earlier switches, as the raise manager's
/// timeout would, so that focus from outside counts again.
fn end_raises(s: &mut Setup) {
    let sequence_id = s.reactor.raise_sequence;
    s.reactor.handle_event(Event::RaiseFocusSent { sequence_id });
    s.reactor.handle_event(Event::RaiseTimeout { sequence_id });
}

/// How the events of an activation reach the reactor. macOS reports the
/// app's global activation in its own order relative to the app's events.
#[derive(Clone, Copy, Debug)]
enum Order {
    GloballyFirst,
    GloballyLast,
}

/// The user activates app `pid`, whose main window is `main`, for example
/// with ⌘-Tab.
fn activate(s: &mut Setup, pid: i32, main: WindowId, order: Order) {
    if let Order::GloballyFirst = order {
        s.reactor.handle_event(Event::ApplicationGloballyActivated(pid));
    }
    s.reactor
        .handle_event(Event::ApplicationMainWindowChanged(pid, Some(main), Quiet::No));
    s.reactor.handle_event(Event::ApplicationActivated(pid, Quiet::No));
    if let Order::GloballyLast = order {
        s.reactor.handle_event(Event::ApplicationGloballyActivated(pid));
    }
}

/// The window's app takes the focus, as a switch's own raise does, so that
/// no focus from outside counts.
fn focus_quietly(s: &mut Setup, wid: WindowId) {
    s.reactor.handle_event(Event::ApplicationGloballyActivated(wid.pid));
    s.reactor.handle_event(Event::ApplicationActivated(wid.pid, Quiet::Yes));
    s.reactor.handle_event(Event::ApplicationMainWindowChanged(
        wid.pid,
        Some(wid),
        Quiet::Yes,
    ));
    assert_eq!(Some(wid), s.reactor.main_window());
}

fn window_at(sys_id: u32, x: f64) -> WindowInfo {
    WindowInfo {
        sys_id: Some(WindowServerId::new(sys_id)),
        frame: rect(x, 100., 50., 50.),
        ..make_window(1)
    }
}

/// Registers app `pid`, which `info` describes, with `windows`, and lists
/// every window in `listed` and the app's as visible.
fn launch(
    s: &mut Setup,
    pid: i32,
    info: AppInfo,
    windows: Vec<WindowInfo>,
    listed: &[WindowId],
) -> Vec<WindowId> {
    let count = windows.len() as u32;
    let events = s.apps.make_app_with_info(pid, info, windows, None, false);
    s.reactor.handle_events(events);
    let wids: Vec<WindowId> = (1..=count).map(|idx| WindowId::new(pid, idx)).collect();
    let mut all = listed.to_vec();
    all.extend(&wids);
    report_visible(s, &all);
    wids
}

fn finder_info() -> AppInfo {
    AppInfo {
        bundle_id: Some("com.apple.finder".into()),
        localized_name: Some("Finder".into()),
    }
}

/// The activations among `requests`.
fn activations(requests: &[Request]) -> Vec<Quiet> {
    requests
        .iter()
        .filter_map(|request| match request {
            Request::Activate(quiet) => Some(*quiet),
            _ => None,
        })
        .collect()
}

/// App 1's window 1, a member of C, and app 2's window, a member of D. C is
/// active, so app 2's window is parked.
struct TwoApps {
    s: Setup,
    c: ContextKey,
    d: ContextKey,
    other: WindowId,
}

fn two_apps() -> TwoApps {
    let mut s = Setup::new(1);
    let other = launch(&mut s, 2, test_app_info(2), vec![window_at(21, 700.)], &[wid(1)])[0];
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[other]);
    s.switch(d);
    s.switch(c);
    end_raises(&mut s);
    assert_eq!(vec![other], s.parked());
    TwoApps { s, c, d, other }
}

/// R24. The user focuses a window that is in no context, of an app that has
/// no member of C. Sugarglider switches to Unsorted and raises the window,
/// which takes the whole screen, and C's member is parked. Both orders of the
/// activation events give the same result.
#[test]
fn r24_focus_on_a_window_in_no_context_switches_to_unsorted() {
    for order in [Order::GloballyFirst, Order::GloballyLast] {
        let mut s = Setup::new(1);
        let loose = launch(&mut s, 2, test_app_info(2), vec![window_at(21, 700.)], &[wid(1)])[0];
        let c = s.create("C", &[wid(1)]);
        s.switch(c);
        end_raises(&mut s);
        assert_eq!(vec![loose], s.parked());
        let mut raises = capture_raises(&mut s);

        activate(&mut s, 2, loose, order);

        assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active(), "{order:?}");
        assert_eq!(vec![Some(loose)], focused(&mut raises), "{order:?}");
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![(loose, screen())], s.tiles(), "{order:?}");
        assert_eq!(screen(), s.frame(loose));
        assert_eq!(vec![wid(1)], s.parked(), "{order:?}");
    }
}

/// R24. While the screen shows Everything, focus on a window of a context
/// switches nothing and raises nothing.
#[test]
fn r24_under_everything_focus_on_a_window_of_a_context_switches_nothing() {
    let TwoApps { mut s, other, .. } = two_apps();
    s.switch(ContextKey::Everything);
    end_raises(&mut s);
    let mut raises = capture_raises(&mut s);

    activate(&mut s, 2, other, Order::GloballyFirst);
    activate(&mut s, 1, wid(1), Order::GloballyLast);

    assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
    assert!(raise_requests(&mut raises).is_empty());
    assert!(s.parked().is_empty());
}

/// R24, R3. A pinned window is a member of every context, so focus on it
/// switches nothing, although it is in D too.
#[test]
fn r24_focus_on_a_pinned_window_switches_nothing() {
    let mut s = Setup::new(1);
    let other = launch(&mut s, 2, test_app_info(2), vec![window_at(21, 700.)], &[wid(1)])[0];
    let c = s.create("C", &[wid(1)]);
    s.create("D", &[other]);
    s.reactor.contexts.pin(&s.desc(other));
    s.switch(c);
    end_raises(&mut s);
    assert!(s.parked().is_empty());
    let mut raises = capture_raises(&mut s);

    activate(&mut s, 2, other, Order::GloballyFirst);

    assert_eq!(c, s.reactor.contexts.active());
    assert!(raise_requests(&mut raises).is_empty());
    assert!(s.parked().is_empty());
}

/// R24, R12 step 5. Focus on a member of the active context keeps the
/// context and counts as the member's latest focus, so the next switch back
/// to C focuses that member.
#[test]
fn r24_focus_on_a_member_counts_for_the_next_switch_to_its_context() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1), wid(2)]);
    let d = s.create("D", &[wid(3)]);
    s.switch(d);
    s.switch(c);
    end_raises(&mut s);

    for wid in [wid(2), wid(1), wid(2)] {
        activate(&mut s, 1, wid, Order::GloballyFirst);
        assert_eq!(c, s.reactor.contexts.active());
    }
    s.switch(d);
    end_raises(&mut s);
    focus_quietly(&mut s, wid(3));
    let mut raises = capture_raises(&mut s);

    s.command(c);

    assert_eq!(vec![Some(wid(2))], focused(&mut raises));
}

/// R24, R20, R38. An app launches while C is active, and macOS reports its
/// global activation last, after its window. The new window joins C, and
/// nothing switches, whether the window reaches the reactor before or after
/// the app's main window change.
#[test]
fn r24_launching_an_app_whose_global_activation_comes_last_never_switches() {
    for window_first in [true, false] {
        let TwoApps { mut s, c, other, .. } = two_apps();
        let mut raises = capture_raises(&mut s);
        let launched = WindowId::new(3, 1);
        let window = window_at(31, 300.);
        s.apps.windows.insert(
            launched,
            WindowState {
                frame: window.frame,
                ..Default::default()
            },
        );
        let events = s.apps.make_app_with_opts(3, vec![], Some(launched), true);
        s.reactor.handle_events(events);
        let created = Event::WindowCreated(launched, window, MouseState::Up);
        let main_changed = Event::ApplicationMainWindowChanged(3, Some(launched), Quiet::No);
        if window_first {
            s.reactor.handle_events(vec![created, main_changed]);
        } else {
            s.reactor.handle_events(vec![main_changed, created]);
        }
        s.reactor.handle_event(Event::ApplicationActivated(3, Quiet::No));
        s.reactor.handle_event(Event::ApplicationGloballyActivated(3));
        report_visible(&mut s, &[wid(1), other, launched]);

        assert_eq!(c, s.reactor.contexts.active(), "window first: {window_first}");
        assert_eq!(vec![id_of(c)], s.reactor.contexts.contexts_of(launched));
        assert_eq!(vec![other], s.parked(), "window first: {window_first}");
        assert!(
            focused(&mut raises).iter().all(|&focus| focus == Some(launched)),
            "window first: {window_first}"
        );
    }
}

/// R24, R14, R20. App 3's main window is only in D, so it is parked while C
/// is active. The app opens a panel on a layer of its own, which reaches the
/// reactor as a new window does: `WindowCreated` comes before the window
/// server's list that gives the panel's layer. The layout doesn't track the
/// panel, so it joins no context and isn't parked. When the user then
/// activates the app, whose main window is still the parked one, Sugarglider
/// switches to D, because the app has no member of C.
#[test]
fn r24_r14_a_new_panel_joins_no_context_and_focus_on_its_app_still_switches() {
    let mut s = Setup::new(1);
    let main = launch(&mut s, 3, test_app_info(3), vec![window_at(31, 700.)], &[wid(1)])[0];
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[main]);
    s.switch(d);
    s.switch(c);
    end_raises(&mut s);
    assert_eq!(vec![main], s.parked());
    let panel = WindowId::new(3, 2);
    let info = window_at(32, 300.);
    s.apps.windows.insert(
        panel,
        WindowState {
            frame: info.frame,
            ..Default::default()
        },
    );

    s.reactor.handle_event(Event::WindowCreated(panel, info, MouseState::Up));
    let mut listed = on_screen(&s, &[wid(1), main, panel]);
    listed.info[2].layer = 3;
    s.reactor.handle_event(Event::WindowsOnScreenUpdated {
        pid: Some(3),
        on_screen: listed,
    });
    s.reactor.handle_event(Event::WindowBecameVisible(panel));
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert!(s.reactor.contexts.is_unsorted(panel));
    assert_eq!(vec![main], s.parked());
    let mut raises = capture_raises(&mut s);
    activate(&mut s, 3, main, Order::GloballyFirst);
    assert_eq!(d, s.reactor.contexts.active());
    assert_eq!(vec![Some(main)], focused(&mut raises));
}

/// R36, R24, R38. A browser makes a new tab its main window before the
/// reactor sees the tab. The tab joins the contexts of its group's main tab,
/// here C and D, and the focus on it switches nothing and raises nothing.
/// The group keeps one tile.
#[test]
fn r36_a_new_tab_that_takes_focus_before_it_is_seen_joins_its_group() {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1), wid(2)]);
    let d = s.create("D", &[wid(1)]);
    s.switch(c);
    end_raises(&mut s);
    focus_quietly(&mut s, wid(1));
    let frames: Vec<CGRect> = s.tiles().into_iter().map(|(_, frame)| frame).collect();
    let mut raises = capture_raises(&mut s);
    super::native_tabs::observe(&mut s, 1, wid(1), &[Some(wid(1))]);
    super::native_tabs::observe(&mut s, 1, wid(3), &[Some(wid(1)), Some(wid(3))]);

    s.reactor
        .handle_event(Event::ApplicationMainWindowChanged(1, Some(wid(3)), Quiet::No));
    let tab = WindowInfo {
        frame: s.frame(wid(1)),
        ..make_window(3)
    };
    s.apps.windows.insert(
        wid(3),
        WindowState {
            frame: tab.frame,
            ..Default::default()
        },
    );
    s.reactor.handle_event(Event::WindowCreated(wid(3), tab, MouseState::Up));
    let on_screen = on_screen(&s, &[wid(1), wid(2), wid(3)]);
    s.reactor
        .handle_event(Event::WindowsOnScreenUpdated { pid: Some(1), on_screen });
    s.reactor.handle_event(Event::WindowBecameVisible(wid(3)));
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert_eq!(c, s.reactor.contexts.active());
    assert_eq!(vec![id_of(c), id_of(d)], s.reactor.contexts.contexts_of(wid(3)));
    assert!(raise_requests(&mut raises).is_empty());
    assert!(s.parked().is_empty());
    let mut after: Vec<CGRect> = s.tiles().into_iter().map(|(_, frame)| frame).collect();
    after.sort_by(|a, b| a.origin.x.total_cmp(&b.origin.x));
    assert_eq!(frames, after);
}

/// R25. After a switch that focus from outside started, the completion of a
/// raise of another window of the sequence, and the end of an older
/// sequence, don't end the wait: an activation of another app still doesn't
/// switch. The completed raise of the focused window does.
#[test]
fn r25_a_raise_of_another_window_or_an_older_sequence_keeps_the_wait() {
    let TwoApps { mut s, c, d, other } = two_apps();
    let mut raises = capture_raises(&mut s);
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(d, s.reactor.contexts.active());
    let [(sequence_id, Some(focus))] = raise_requests(&mut raises)[..] else {
        panic!()
    };
    assert_eq!(other, focus);
    s.apps.simulate_until_quiet(&mut s.reactor);

    s.reactor.handle_event(Event::RaiseCompleted { window_id: wid(1), sequence_id });
    s.reactor.handle_event(Event::RaiseTimeout { sequence_id: sequence_id - 1 });
    s.reactor.handle_event(Event::RaiseRequestFailed {
        windows: vec![other],
        sequence_id: sequence_id - 1,
        quiet: Quiet::No,
    });
    activate(&mut s, 1, wid(1), Order::GloballyLast);
    assert_eq!(d, s.reactor.contexts.active());

    s.reactor.handle_event(Event::RaiseCompleted { window_id: other, sequence_id });
    activate(&mut s, 1, wid(1), Order::GloballyLast);
    assert_eq!(c, s.reactor.contexts.active());
}

/// R25. The wait for a switch's raise ends when the window it focuses is
/// destroyed, or when that window's app quits, because no event of the raise
/// can come any more.
#[test]
fn r25_the_wait_ends_when_the_focused_window_or_its_app_goes_away() {
    for gone in ["window", "app"] {
        let TwoApps { mut s, c, d, other } = two_apps();
        let _raises = capture_raises(&mut s);
        activate(&mut s, 2, other, Order::GloballyFirst);
        assert_eq!(d, s.reactor.contexts.active(), "{gone}");
        s.apps.simulate_until_quiet(&mut s.reactor);
        activate(&mut s, 1, wid(1), Order::GloballyLast);
        assert_eq!(d, s.reactor.contexts.active(), "{gone}");

        match gone {
            "window" => s.close(other),
            _ => {
                s.apps.windows.remove(&other);
                s.reactor.handle_event(Event::ApplicationTerminated(2));
                s.reactor.handle_event(Event::ApplicationThreadTerminated(2));
            }
        }
        s.apps.simulate_until_quiet(&mut s.reactor);
        activate(&mut s, 1, wid(1), Order::GloballyLast);

        assert_eq!(c, s.reactor.contexts.active(), "{gone}");
    }
}

/// App 1's windows 1 and 2 and app 2's window, all showing under
/// Everything. C holds window 1, and D holds the other two. Window 1 has the
/// focus, quietly.
fn three_windows_under_everything() -> (Setup, ContextKey, ContextKey, WindowId) {
    let mut s = Setup::new(2);
    let listed = [wid(1), wid(2)];
    let other = launch(&mut s, 2, test_app_info(2), vec![window_at(21, 700.)], &listed)[0];
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[wid(2), other]);
    focus_quietly(&mut s, wid(1));
    assert!(s.parked().is_empty());
    (s, c, d, other)
}

/// R25. A switch to C raises nothing, because C's window has the focus
/// already, and it parks two windows. It waits for the echo of both parking
/// writes: after one echo, an activation of app 2 still doesn't switch.
/// After both, it does.
#[test]
fn r25_a_switch_that_raises_nothing_waits_for_the_echo_of_every_window_it_parked() {
    let (mut s, c, d, other) = three_windows_under_everything();
    let mut raises = capture_raises(&mut s);

    s.command(c);

    assert!(focused(&mut raises).iter().all(Option::is_none));
    let (app_2, app_1): (Vec<Request>, Vec<Request>) =
        s.apps.requests().into_iter().partition(
            |request| matches!(request, Request::SetWindowFrame(wid, ..) if wid.pid == 2),
        );
    assert_eq!(1, frame_writes(&app_1, wid(2)).len());
    assert_eq!(1, frame_writes(&app_2, other).len());
    answer(&mut s, app_2);
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(c, s.reactor.contexts.active(), "window 2's echo hasn't arrived");

    answer(&mut s, app_1);
    s.apps.simulate_until_quiet(&mut s.reactor);
    activate(&mut s, 2, other, Order::GloballyLast);
    assert_eq!(d, s.reactor.contexts.active());
}

/// R25. A same-context switch must wait for a strict repark when an app moves
/// a window again while an earlier routine repark is still awaiting readback.
#[test]
fn r25_a_strict_repark_without_a_raise_holds_external_focus_until_readback() {
    let (mut s, c, d, other) = three_windows_under_everything();
    let mut raises = capture_raises(&mut s);
    s.switch(c);
    assert!(s.reactor.switch_guard.since.is_none());

    let moved = rect(300., 200., 50., 50.);
    let previous = s.reactor.windows[&other].last_sent_txid;
    s.apps.windows.get_mut(&other).unwrap().frame = moved;
    s.reactor.handle_event(Event::WindowFrameChanged(
        other,
        moved,
        previous,
        Requested(false),
        None,
    ));
    let routine = s.apps.requests();
    assert_eq!(1, frame_writes(&routine, other).len());
    s.reactor.parked.get_mut(&other).unwrap().observed = moved;
    assert_eq!(c, s.reactor.contexts.active());
    assert!(s.reactor.next_parking_deadline().is_some());

    s.command(c);
    let strict = s.apps.requests();
    assert_eq!(1, frame_writes(&strict, other).len());
    assert!(focused(&mut raises).iter().all(Option::is_none));
    assert!(s.reactor.next_guard_deadline().is_none());
    assert!(s.reactor.next_parking_deadline().is_some());

    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(c, s.reactor.contexts.active());

    answer(&mut s, strict);
    assert_eq!(d, s.reactor.contexts.active());
}

/// R25. A second switch can require a new parking echo and a new Finder
/// activation. The first Finder deadline ends neither wait.
#[test]
fn r25_an_older_finder_deadline_keeps_a_later_strict_repark_guarded() {
    let (mut s, _c, d, other) = three_windows_under_everything();
    let _raises = capture_raises(&mut s);
    launch(&mut s, 9, finder_info(), vec![], &[wid(1), wid(2), other]);
    let empty = s.create("Empty", &[]);
    s.command(empty);
    let initial = s.apps.requests();
    answer(&mut s, initial);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert!(s.reactor.next_parking_deadline().is_none());
    let first_since = s.reactor.switch_guard.since.unwrap();
    let first_deadline = first_since + Duration::from_secs(2);

    let moved = rect(300., 200., 50., 50.);
    let previous = s.reactor.windows[&other].last_sent_txid;
    s.apps.windows.get_mut(&other).unwrap().frame = moved;
    s.reactor.handle_event(Event::WindowFrameChanged(
        other,
        moved,
        previous,
        Requested(false),
        None,
    ));
    let routine = s.apps.requests();
    assert_eq!(1, frame_writes(&routine, other).len());
    s.reactor.parked.get_mut(&other).unwrap().observed = moved;

    s.command(empty);
    let strict = s.apps.requests();
    assert_eq!(1, frame_writes(&strict, other).len());
    let second_deadline = s.reactor.next_guard_deadline().unwrap();
    assert!(second_deadline > first_deadline);
    let strict_deadline = s.reactor.next_parking_deadline().unwrap();
    assert!(strict_deadline > first_deadline);

    s.reactor.guard_deadline_tick(first_deadline);
    assert_eq!(Some(second_deadline), s.reactor.next_guard_deadline());
    assert_eq!(Some(strict_deadline), s.reactor.next_switch_deadline());
    s.reactor.guard_deadline_tick(first_deadline + Duration::from_nanos(1));
    assert_eq!(Some(strict_deadline), s.reactor.next_switch_deadline());
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(empty, s.reactor.contexts.active());

    answer(&mut s, strict);
    assert_eq!(empty, s.reactor.contexts.active());
    s.reactor.guard_deadline_tick(second_deadline);
    assert_eq!(d, s.reactor.contexts.active());
}

/// Leave window 1 awaiting a strict repark alongside a Finder wait. Its app
/// has reported a different frame, and no answer to the strict write will
/// arrive before the caller releases the window.
fn strict_repark_with_finder_wait() -> (Setup, ContextKey, ContextKey, WindowId, ContextKey) {
    let (mut s, c, d, other) = three_windows_under_everything();
    let _raises = capture_raises(&mut s);
    launch(&mut s, 9, finder_info(), vec![], &[wid(1), wid(2), other]);
    let empty = s.create("Empty", &[]);
    s.command(empty);
    let initial = s.apps.requests();
    answer(&mut s, initial);
    s.apps.simulate_until_quiet(&mut s.reactor);
    let first_deadline = s.reactor.next_guard_deadline().unwrap();

    let moved = rect(300., 200., 50., 50.);
    let previous = s.reactor.windows[&wid(1)].last_sent_txid;
    s.apps.windows.get_mut(&wid(1)).unwrap().frame = moved;
    s.reactor.handle_event(Event::WindowFrameChanged(
        wid(1),
        moved,
        previous,
        Requested(false),
        None,
    ));
    let routine = s.apps.requests();
    assert_eq!(1, frame_writes(&routine, wid(1)).len());
    s.reactor.parked.get_mut(&wid(1)).unwrap().observed = moved;

    s.command(empty);
    let strict = s.apps.requests();
    assert_eq!(1, frame_writes(&strict, wid(1)).len());
    assert!(s.reactor.next_parking_deadline().unwrap() > first_deadline);
    assert!(s.reactor.next_guard_deadline().is_some());
    assert!(s.reactor.next_parking_deadline().is_some());
    (s, c, d, other, empty)
}

/// R25. A direct switch can release a window whose strict repark echo still
/// holds an older switch. Its unpark readback is not a parking confirmation.
#[test]
fn r25_direct_switch_cancels_the_released_windows_old_parking_echo() {
    let (mut s, c, d, other, _empty) = strict_repark_with_finder_wait();
    s.reactor.handle_event(Event::ActivateFailed(9));
    assert_eq!(Some(wid(1)), s.reactor.main_window());

    s.command(c);

    assert!(!s.reactor.parked.contains_key(&wid(1)));
    assert!(s.reactor.next_switch_deadline().is_none());
    assert_eq!(c, s.reactor.contexts.active());
    // Leave both the old strict write and the new unpark write unanswered.
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(d, s.reactor.contexts.active());
}

/// R25. Releasing an old echo during a direct switch cannot apply buffered
/// outside focus before the new focusing raise has been added to the guard.
#[test]
fn r25_releasing_an_echo_during_a_switch_keeps_its_new_raise_guarded() {
    let (mut s, c, d, other, empty) = strict_repark_with_finder_wait();
    s.reactor.handle_event(Event::ActivateFailed(9));
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(empty, s.reactor.contexts.active());

    s.command(c);

    assert_eq!(c, s.reactor.contexts.active());
    assert!(s.reactor.next_guard_deadline().is_some());
    end_raises(&mut s);
    assert_eq!(d, s.reactor.contexts.active());
}

/// R25. A non-switch unpark cancels the old parking echo. Buffered focus is
/// reconsidered at the next event boundary, after the unpark has finished.
#[test]
fn r25_non_switch_unpark_cancels_the_released_windows_old_parking_echo() {
    let (mut s, _c, d, other, _empty) = strict_repark_with_finder_wait();
    s.reactor.handle_event(Event::ActivateFailed(9));
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_ne!(d, s.reactor.contexts.active());

    s.reactor.unpark_windows(&[wid(1)]);
    assert!(!s.reactor.parked.contains_key(&wid(1)));
    assert!(s.reactor.next_switch_deadline().is_none());
    s.reactor.handle_event(Event::ApplicationActivated(2, Quiet::Yes));
    assert_eq!(d, s.reactor.contexts.active());
}

/// R25. A finished Finder wait keeps its start time while a parking echo is
/// outstanding. Releasing the last parked window must clear that time even
/// when no focus was buffered, so a later Finder wait gets a fresh deadline.
#[test]
fn r25_releasing_the_last_echo_without_buffered_focus_resets_guard_time() {
    let (mut s, c, _d, _other, empty) = strict_repark_with_finder_wait();
    let old_deadline = s.reactor.next_guard_deadline().unwrap();
    s.reactor.handle_event(Event::ActivateFailed(9));
    assert_eq!(
        Some(old_deadline),
        s.reactor.switch_guard.since.map(|since| since + Duration::from_secs(2))
    );
    assert!(s.reactor.next_guard_deadline().is_none());

    s.command(c);
    assert!(s.reactor.switch_guard.since.is_none());
    s.command(empty);
    assert!(s.reactor.next_guard_deadline().unwrap() > old_deadline);
}

/// R25. A second focusing raise gets its own two-second wait even when it
/// starts just before the first one's deadline.
#[test]
fn r25_overlapping_raise_gets_a_fresh_deadline() {
    let TwoApps { mut s, c, d, other } = two_apps();
    let _raises = capture_raises(&mut s);
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(d, s.reactor.contexts.active());
    s.reactor.switch_guard.since = Some(Instant::now() - Duration::from_millis(1900));
    let old_deadline = s.reactor.next_guard_deadline().unwrap();

    s.command(c);

    let new_deadline = s.reactor.next_guard_deadline().unwrap();
    assert!(new_deadline > old_deadline);
    s.reactor.guard_deadline_tick(old_deadline);
    assert_eq!(Some(new_deadline), s.reactor.next_guard_deadline());
}

/// R25. A missing parking echo restores Everything at the deadline. A missing
/// Finder activation only ends the focus wait.
#[test]
fn r25_the_2_second_fallback_also_ends_a_wait_for_echoes_or_for_finder() {
    for waits_for in ["echoes", "Finder"] {
        let (mut s, c, d, other) = three_windows_under_everything();
        let _raises = capture_raises(&mut s);
        let target = if waits_for == "Finder" {
            launch(&mut s, 9, finder_info(), vec![], &[wid(1), wid(2), other]);
            s.create("Empty", &[])
        } else {
            c
        };

        s.command(target);
        if waits_for == "Finder" {
            // The parking writes are answered, but Finder's activation
            // never arrives.
            let requests = s.apps.requests();
            assert_eq!(vec![Quiet::Yes], activations(&requests));
            answer(&mut s, requests);
            s.apps.simulate_until_quiet(&mut s.reactor);
        }
        let deadline = if waits_for == "echoes" {
            s.reactor.next_parking_deadline().unwrap()
        } else {
            s.reactor.next_guard_deadline().unwrap()
        };
        s.reactor.guard_deadline_tick(deadline - Duration::from_secs(1));
        activate(&mut s, 2, other, Order::GloballyFirst);
        assert_eq!(target, s.reactor.contexts.active(), "{waits_for}");

        if waits_for == "echoes" {
            s.reactor.parking_deadline_tick(deadline);
        } else {
            s.reactor.guard_deadline_tick(deadline);
        }
        activate(&mut s, 2, other, Order::GloballyLast);
        assert_eq!(
            if waits_for == "echoes" {
                ContextKey::Everything
            } else {
                d
            },
            s.reactor.contexts.active(),
            "{waits_for}"
        );
    }
}

/// R25. A visibility refresh just before the guard deadline cannot postpone
/// a missing Finder activation or focusing raise until the following refresh.
#[test]
fn r25_missing_finder_or_raise_uses_the_exact_guard_deadline() {
    for waits_for in ["Finder", "raise"] {
        let (mut s, _c, d, other) = three_windows_under_everything();
        let _raises = capture_raises(&mut s);
        let target = if waits_for == "Finder" {
            launch(&mut s, 9, finder_info(), vec![], &[wid(1), wid(2), other]);
            s.create("Empty", &[])
        } else {
            d
        };

        s.command(target);
        let requests = s.apps.requests();
        answer(&mut s, requests);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert!(s.reactor.next_parking_deadline().is_none(), "{waits_for}");

        let since = s.reactor.switch_guard.since.unwrap();
        let deadline = since + Duration::from_secs(2);
        let next_visibility_refresh = since + Duration::from_millis(1999);
        assert!(next_visibility_refresh < deadline);
        assert_eq!(Some(deadline), s.reactor.next_guard_deadline(), "{waits_for}");
        assert_eq!(Some(deadline), s.reactor.next_switch_deadline(), "{waits_for}");
        s.reactor.guard_deadline_tick(next_visibility_refresh);
        assert_eq!(Some(deadline), s.reactor.next_switch_deadline(), "{waits_for}");
        s.reactor.guard_deadline_tick(deadline);
        assert!(s.reactor.next_switch_deadline().is_none(), "{waits_for}");
    }
}

/// R25. A second switch starts before the first one's raise ends. The end of
/// the first switch's raise doesn't end the wait for the second's, so an
/// activation of app 2 doesn't switch until the second raise completes.
#[test]
fn r25_a_second_switch_waits_for_its_own_raise() {
    let TwoApps { mut s, c, d, other } = two_apps();
    let mut raises = capture_raises(&mut s);
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(d, s.reactor.contexts.active());
    let [(first, Some(_))] = raise_requests(&mut raises)[..] else {
        panic!()
    };

    s.command(c);
    let [(second, Some(focus))] = raise_requests(&mut raises)[..] else {
        panic!()
    };
    assert_eq!(wid(1), focus);
    assert!(second > first);
    s.apps.simulate_until_quiet(&mut s.reactor);

    s.reactor.handle_event(Event::RaiseCompleted {
        window_id: other,
        sequence_id: first,
    });
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(c, s.reactor.contexts.active());

    s.reactor.handle_event(Event::RaiseCompleted {
        window_id: wid(1),
        sequence_id: second,
    });
    activate(&mut s, 2, other, Order::GloballyLast);
    assert_eq!(d, s.reactor.contexts.active());
}

/// R25. The failures and timeouts of the raises before the switch's focusing
/// raise don't end the wait for the switch; the failure of the focusing
/// raise itself does.
#[test]
fn r25_the_batches_before_the_focusing_raise_dont_end_the_wait() {
    let TwoApps { mut s, c, d, other } = two_apps();
    let mut raises = capture_raises(&mut s);
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(d, s.reactor.contexts.active());
    let [(sequence_id, _)] = raise_requests(&mut raises)[..] else {
        panic!()
    };
    s.apps.simulate_until_quiet(&mut s.reactor);

    s.reactor.handle_event(Event::RaiseRequestFailed {
        windows: vec![wid(1)],
        sequence_id,
        quiet: Quiet::Yes,
    });
    s.reactor.handle_event(Event::RaiseTimeout { sequence_id });
    activate(&mut s, 1, wid(1), Order::GloballyLast);
    assert_eq!(
        d,
        s.reactor.contexts.active(),
        "the focusing raise hasn't gone out"
    );

    // A failure of another raise of the sequence still doesn't end the wait
    // once the focusing raise is on its way.
    s.reactor.handle_event(Event::RaiseFocusSent { sequence_id });
    s.reactor.handle_event(Event::RaiseRequestFailed {
        windows: vec![wid(1)],
        sequence_id,
        quiet: Quiet::Yes,
    });
    activate(&mut s, 1, wid(1), Order::GloballyLast);
    assert_eq!(
        d,
        s.reactor.contexts.active(),
        "the focusing raise hasn't failed"
    );

    s.reactor.handle_event(Event::RaiseRequestFailed {
        windows: vec![other],
        sequence_id,
        quiet: Quiet::No,
    });
    activate(&mut s, 1, wid(1), Order::GloballyFirst);
    assert_eq!(c, s.reactor.contexts.active());
}

/// R25. An identical raise request replaces the queued one with a newer id,
/// so the wait for the focusing raise ends on a raise of the switch's window
/// that carries an id at least as new as the switch's.
#[test]
fn r25_a_raise_with_a_newer_identifier_ends_the_wait() {
    let TwoApps { mut s, c, d, other } = two_apps();
    let mut raises = capture_raises(&mut s);
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(d, s.reactor.contexts.active());
    let [(first, _)] = raise_requests(&mut raises)[..] else {
        panic!()
    };
    s.apps.simulate_until_quiet(&mut s.reactor);

    s.reactor.handle_event(Event::RaiseFocusSent { sequence_id: first + 1 });
    s.reactor.handle_event(Event::RaiseCompleted {
        window_id: other,
        sequence_id: first + 1,
    });
    activate(&mut s, 1, wid(1), Order::GloballyFirst);

    assert_eq!(c, s.reactor.contexts.active());
}

/// R25. A second switch extends the wait of the first: after the second
/// switch's raise ends, the echo the first switch still waits for keeps
/// holding focus back, and the focus that arrived meanwhile applies when the
/// echo arrives.
#[test]
fn r25_a_second_switch_extends_the_wait_of_the_first() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[wid(2)]);
    focus_quietly(&mut s, wid(1));
    let mut raises = capture_raises(&mut s);

    // C's window has the focus already, so this switch raises nothing and
    // waits for the echoes of the windows it parks.
    s.command(c);
    assert!(focused(&mut raises).iter().all(Option::is_none));
    let first = s.apps.requests();
    assert_eq!(1, frame_writes(&first, wid(2)).len());
    assert_eq!(1, frame_writes(&first, wid(3)).len());

    // The second switch raises its window, and keeps waiting for the echoes
    // of the first.
    s.command(d);
    let [(sequence_id, Some(focus))] = raise_requests(&mut raises)[..] else {
        panic!()
    };
    assert_eq!(wid(2), focus);
    let second = s.apps.requests();
    s.reactor.handle_event(Event::RaiseFocusSent { sequence_id });
    s.reactor.handle_event(Event::RaiseCompleted { window_id: focus, sequence_id });
    s.reactor
        .handle_event(Event::ApplicationMainWindowChanged(1, Some(wid(3)), Quiet::No));
    assert_eq!(
        d,
        s.reactor.contexts.active(),
        "window 3's echo is still to come"
    );

    answer(&mut s, first);
    s.apps.simulate_until_quiet(&mut s.reactor);
    answer(&mut s, second);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
}

/// R24, R38. Focus on a window of another context before startup completes
/// changes nothing: switching during registration would fight R38. Once
/// startup completes, the same focus switches.
#[test]
fn r24_focus_during_registration_does_not_switch() {
    let mut s = Setup::on(vec![screen()], vec![Some(space())]);
    s.reactor.handle_events(s.apps.make_app(1, make_windows(1)));
    let d = ContextKey::Named(s.reactor.contexts.create("D").unwrap());
    let old = WindowId::new(9, 1);
    let desc = WindowDesc {
        wid: old,
        bundle_id: Some("com.testapp2".into()),
        app_name: Some("TestApp2".into()),
        title: "Window1".into(),
        window_server_id: Some(WindowServerId::new(21)),
    };
    s.reactor.contexts.add_window(id_of(d), &desc).unwrap();
    s.reactor.contexts.window_closed(old);
    s.reactor.contexts.app_terminated(old.pid);
    s.apps.windows.insert(
        WindowId::new(2, 1),
        WindowState {
            frame: rect(700., 100., 50., 50.),
            ..Default::default()
        },
    );
    s.reactor.handle_events(s.apps.make_app(2, vec![window_at(21, 700.)]));
    let doc = WindowId::new(2, 1);
    report_visible(&mut s, &[wid(1), doc]);
    assert!(s.reactor.contexts.is_member(d, doc));
    let c = s.create("C", &[wid(1)]);
    s.switch(c);
    end_raises(&mut s);
    let _raises = capture_raises(&mut s);

    activate(&mut s, 2, doc, Order::GloballyFirst);
    assert_eq!(c, s.reactor.contexts.active(), "focus during registration");

    s.reactor.handle_event(Event::StartupComplete);
    s.apps.simulate_until_quiet(&mut s.reactor);
    end_raises(&mut s);
    activate(&mut s, 2, doc, Order::GloballyLast);
    assert_eq!(d, s.reactor.contexts.active());
}

/// R24, R38. Focus names a window the reactor hasn't seen yet, and waits.
/// When the window appears without being the main window any more, the focus
/// does nothing.
#[test]
fn r24_focus_that_waits_does_nothing_when_the_window_is_no_longer_the_main_window() {
    let TwoApps { mut s, c, .. } = two_apps();
    let e = s.create("E", &[]);
    let _raises = capture_raises(&mut s);
    let old = WindowId::new(9, 1);
    let desc = WindowDesc {
        wid: old,
        bundle_id: Some("com.testapp3".into()),
        app_name: Some("TestApp3".into()),
        title: "Window1".into(),
        window_server_id: Some(WindowServerId::new(31)),
    };
    s.reactor.contexts.add_window(id_of(e), &desc).unwrap();
    s.reactor.contexts.window_closed(old);
    s.reactor.contexts.app_terminated(old.pid);
    let launched = WindowId::new(3, 1);
    let window = window_at(31, 300.);
    s.apps.windows.insert(
        launched,
        WindowState {
            frame: window.frame,
            ..Default::default()
        },
    );
    s.reactor.handle_events(s.apps.make_app_with_opts(3, vec![], None, true));
    s.reactor.handle_event(Event::ApplicationGloballyActivated(3));
    s.reactor
        .handle_event(Event::ApplicationMainWindowChanged(3, Some(launched), Quiet::No));
    // The app changes its main window again, quietly, before the reactor
    // sees the first one.
    s.reactor.handle_event(Event::ApplicationMainWindowChanged(
        3,
        Some(WindowId::new(3, 2)),
        Quiet::Yes,
    ));

    s.reactor.handle_event(Event::WindowCreated(launched, window, MouseState::Up));
    let on_screen = on_screen(&s, &[wid(1), launched]);
    s.reactor
        .handle_event(Event::WindowsOnScreenUpdated { pid: Some(3), on_screen });

    assert!(s.reactor.contexts.is_member(e, launched));
    assert_eq!(c, s.reactor.contexts.active());
}

/// R25. A focus that arrived during the wait applies when the wait ends only
/// while its window still has the focus: the user may have moved on.
#[test]
fn r25_focus_that_arrived_during_the_wait_applies_only_while_it_still_has_focus() {
    let TwoApps { mut s, c, d, other } = two_apps();
    let mut raises = capture_raises(&mut s);
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(d, s.reactor.contexts.active());
    let [(sequence_id, _)] = raise_requests(&mut raises)[..] else {
        panic!()
    };
    s.apps.simulate_until_quiet(&mut s.reactor);

    // The user activates app 1 during the wait, and moves on to app 2.
    activate(&mut s, 1, wid(1), Order::GloballyFirst);
    s.reactor.handle_event(Event::ApplicationGloballyDeactivated(1));
    s.reactor.handle_event(Event::ApplicationDeactivated(1));

    s.reactor.handle_event(Event::RaiseFocusSent { sequence_id });
    s.reactor.handle_event(Event::RaiseCompleted { window_id: other, sequence_id });

    assert_eq!(
        d,
        s.reactor.contexts.active(),
        "window 1 no longer has the focus"
    );
    assert!(raise_requests(&mut raises).is_empty());
    assert_ne!(c, s.reactor.contexts.active());
}

/// R40. The user activates app 1, whose main window 2 is parked. Its member
/// of C, window 1, is minimized, so there is no visible member to raise, and
/// the app has a member of C, so Sugarglider doesn't switch either.
#[test]
fn r40_an_app_whose_member_of_the_active_context_is_minimized_neither_raises_nor_switches() {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[wid(2)]);
    s.switch(d);
    s.switch(c);
    end_raises(&mut s);
    assert_eq!(vec![wid(2)], s.parked());
    report_visible(&mut s, &[wid(2)]);
    assert!(s.tiles().is_empty());
    let mut raises = capture_raises(&mut s);

    activate(&mut s, 1, wid(2), Order::GloballyFirst);

    assert_eq!(c, s.reactor.contexts.active());
    assert!(raise_requests(&mut raises).is_empty());
    assert_eq!(vec![wid(2)], s.parked());
}

/// R40, R3. A visible pinned window of the app is a member of the active
/// context. When it is the app's most recently focused member, activating
/// the app with a parked main window raises it.
#[test]
fn r40_a_visible_pinned_window_of_the_app_is_a_member_to_raise() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(3)]);
    let d = s.create("D", &[wid(2)]);
    s.reactor.contexts.pin(&s.desc(wid(1)));
    s.switch(d);
    s.switch(c);
    end_raises(&mut s);
    assert_eq!(vec![wid(2)], s.parked());
    s.reactor.contexts.window_focused(wid(3));
    s.reactor.contexts.window_focused(wid(1));
    let mut raises = capture_raises(&mut s);

    activate(&mut s, 1, wid(2), Order::GloballyFirst);

    assert_eq!(c, s.reactor.contexts.active());
    assert_eq!(vec![Some(wid(1))], focused(&mut raises));
    assert_eq!(vec![wid(2)], s.parked());
}

/// R12 step 6, R37. Moving C's last window to D parks it and leaves no
/// window to focus, so Finder is activated quietly and nothing is raised.
#[test]
fn r12_step_6_moving_the_last_member_out_activates_finder_quietly() {
    let mut s = Setup::new(1);
    launch(&mut s, 9, finder_info(), vec![], &[wid(1)]);
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[]);
    s.switch(c);
    end_raises(&mut s);
    focus_quietly(&mut s, wid(1));
    let mut raises = capture_raises(&mut s);

    s.reactor.handle_event(Event::Command(Command::Context(
        ContextCommand::MoveWindowToContext(ContextRef::Id(id_of(d))),
    )));

    let requests = s.apps.requests();
    assert_eq!(vec![Quiet::Yes], activations(&requests));
    assert_eq!(vec![corner(screen().size)], frame_writes(&requests, wid(1)));
    assert!(focused(&mut raises).iter().all(Option::is_none));
    answer(&mut s, requests);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(vec![wid(1)], s.parked());
    assert_eq!(c, s.reactor.contexts.active());
}

/// R12 step 6. Without Finder running, a switch to a context without windows
/// activates no app and raises nothing, and it parks every window.
#[test]
fn r12_step_6_without_finder_a_switch_to_an_empty_context_activates_nothing() {
    let TwoApps { mut s, other, .. } = two_apps();
    let empty = s.create("Empty", &[]);
    let mut raises = capture_raises(&mut s);

    s.command(empty);

    let requests = s.apps.requests();
    assert!(activations(&requests).is_empty());
    assert!(focused(&mut raises).iter().all(Option::is_none));
    answer(&mut s, requests);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(vec![wid(1), other], s.parked());
    assert_eq!(empty, s.reactor.contexts.active());
}

/// R24, R33. While Sugarglider is about to stop managing the Space, the
/// screen shows Everything although C stays the active context. Focus on a
/// window of D switches nothing and raises nothing there.
#[test]
fn r24_r33_a_screen_that_shows_everything_before_its_space_is_turned_off_never_switches() {
    let TwoApps { mut s, c, other, .. } = two_apps();
    s.reactor.handle_event(Event::ShowEverythingOn(vec![space()]));
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert!(s.parked().is_empty());
    let mut raises = capture_raises(&mut s);

    activate(&mut s, 2, other, Order::GloballyFirst);
    activate(&mut s, 1, wid(1), Order::GloballyLast);

    assert_eq!(c, s.reactor.contexts.active());
    assert!(raise_requests(&mut raises).is_empty());
    assert!(s.parked().is_empty());
}

/// R12 steps 5 and 6, R16. C is active when Sugarglider starts, and the
/// frontmost app's main window, window 2, isn't in C. When startup completes,
/// the context is applied again, and window 2 is parked. As after a switch
/// that parks it, the focus moves to C's member, window 1, so keystrokes
/// don't go to a parked window.
#[test]
fn r12_r16_the_apply_at_startup_moves_the_focus_off_the_window_it_parks() {
    let mut s = Setup::on(vec![screen()], vec![Some(space())]);
    let c = ContextKey::Named(s.reactor.contexts.create("C").unwrap());
    let gone = WindowId::new(90, 1);
    let desc = WindowDesc {
        wid: gone,
        bundle_id: Some("com.testapp1".into()),
        app_name: Some("TestApp1".into()),
        title: "Window1".into(),
        window_server_id: None,
    };
    s.reactor.contexts.add_window(id_of(c), &desc).unwrap();
    s.reactor.contexts.window_closed(gone);
    s.reactor.contexts.app_terminated(gone.pid);
    s.reactor.contexts.switch_to(c).unwrap();
    let mut raises = capture_raises(&mut s);
    let events = s.apps.make_app_with_opts(1, make_windows(2), Some(wid(2)), true);
    s.reactor.handle_events(events);
    s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
    assert_eq!(Some(wid(2)), s.reactor.main_window());
    raise_requests(&mut raises);

    s.reactor.handle_event(Event::StartupComplete);
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert_eq!(vec![wid(2)], s.parked());
    assert_eq!(vec![(wid(1), screen())], s.tiles());
    let focus: Vec<WindowId> = focused(&mut raises).into_iter().flatten().collect();
    assert_eq!(vec![wid(1)], focus);
}

/// R12 steps 5 and 6, R16. A config reload turns contexts on while the
/// focused window, window 2, isn't in C. The reload applies C, which parks
/// window 2, and the focus moves to C's member, window 1.
#[test]
fn r12_step_5_a_reload_that_turns_contexts_on_moves_the_focus_off_the_window_it_parks() {
    let mut s = Setup::on(vec![screen()], vec![Some(space())]);
    s.reactor.handle_events(s.apps.make_app(1, make_windows(2)));
    s.reactor.handle_event(Event::StartupComplete);
    s.apps.simulate_until_quiet(&mut s.reactor);
    let c = s.create("C", &[wid(1)]);
    s.switch(c);
    end_raises(&mut s);
    s.reactor.handle_event(Event::ConfigChanged(config(false)));
    s.apps.simulate_until_quiet(&mut s.reactor);
    focus_quietly(&mut s, wid(2));
    assert!(s.parked().is_empty());
    let mut raises = capture_raises(&mut s);

    s.reactor.handle_event(Event::ConfigChanged(config(true)));
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert_eq!(c, s.reactor.contexts.active());
    assert_eq!(vec![wid(2)], s.parked());
    assert_eq!(vec![Some(wid(1))], focused(&mut raises));
    assert_eq!(vec![(wid(1), screen())], s.tiles());
    assert_eq!(screen(), s.frame(wid(1)));
}

/// R12 step 6, R25. A switch to a context without windows activates Finder.
/// Finder's activation then arrives labeled as the user's, as it is when it
/// comes after the app thread stops labeling it quiet. It is the switch's own
/// activation, which ends the wait, so it doesn't switch back to C, the
/// context of Finder's parked main window.
#[test]
fn r12_step_6_r25_finders_own_activation_never_switches_back() {
    let TwoApps { mut s, c, .. } = two_apps();
    let finder = launch(&mut s, 9, finder_info(), vec![window_at(91, 900.)], &[wid(1)])[0];
    assert!(s.reactor.contexts.is_member(c, finder));
    let empty = s.create("Empty", &[]);
    let _raises = capture_raises(&mut s);
    s.command(empty);
    let requests = s.apps.requests();
    assert_eq!(vec![Quiet::Yes], activations(&requests));
    answer(&mut s, requests);
    s.apps.simulate_until_quiet(&mut s.reactor);

    s.reactor.handle_event(Event::ApplicationGloballyActivated(9));
    s.reactor
        .handle_event(Event::ApplicationMainWindowChanged(9, Some(finder), Quiet::No));
    s.reactor.handle_event(Event::ApplicationActivated(9, Quiet::No));

    assert_eq!(empty, s.reactor.contexts.active());
}

/// R12 step 6. Finder is already the frontmost app, so a switch that leaves
/// no window to focus activates nothing, and nothing waits for an activation.
#[test]
fn r12_step_6_a_finder_that_is_already_frontmost_gets_no_activation() {
    let mut s = Setup::new(1);
    launch(&mut s, 9, finder_info(), vec![], &[wid(1)]);
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[]);
    s.switch(c);
    end_raises(&mut s);
    focus_quietly(&mut s, wid(1));
    let _raises = capture_raises(&mut s);
    s.reactor.handle_event(Event::ApplicationGloballyActivated(9));

    s.command(d);

    let requests = s.apps.requests();
    assert!(activations(&requests).is_empty(), "{requests:?}");
    answer(&mut s, requests);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(d, s.reactor.contexts.active());

    activate(&mut s, 1, wid(1), Order::GloballyFirst);
    assert_eq!(c, s.reactor.contexts.active());
}

/// R12 step 6, R25. The activation of Finder can fail. The switch stops
/// waiting for it there and then, so focus from outside counts again.
#[test]
fn r12_step_6_a_failed_activation_of_finder_ends_the_wait() {
    let TwoApps { mut s, c, .. } = two_apps();
    launch(&mut s, 9, finder_info(), vec![window_at(91, 900.)], &[wid(1)]);
    let empty = s.create("Empty", &[]);
    let _raises = capture_raises(&mut s);

    s.command(empty);

    let requests = s.apps.requests();
    assert_eq!(vec![Quiet::Yes], activations(&requests));
    answer(&mut s, requests);
    s.apps.simulate_until_quiet(&mut s.reactor);

    s.reactor.handle_event(Event::ActivateFailed(9));
    activate(&mut s, 1, wid(1), Order::GloballyFirst);

    assert_eq!(c, s.reactor.contexts.active());
}

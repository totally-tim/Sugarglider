// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reactor tests for focus from outside the active context (R24, R25, R40)
//! and the focus of a switch (R12). The raise manager's channel is replaced
//! by one the test reads, and the test sends the activation and main
//! window events that the raises would cause.

use test_log::test;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::Span;

use super::*;
use crate::actor::reactor::testing::test_app_info;
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

fn window_at(sys_id: u32, x: f64) -> WindowInfo {
    WindowInfo {
        sys_id: Some(WindowServerId::new(sys_id)),
        frame: rect(x, 100., 50., 50.),
        ..make_window(1)
    }
}

/// Registers app `pid` that `info` describes, with `windows`, and lists
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

/// R24, R12 step 5. The user activates app 2, whose window is only in D.
/// Sugarglider switches to D and raises that window, whichever order the
/// activation events arrive in.
#[test]
fn r24_focus_on_a_window_of_another_context_switches_and_focuses_it() {
    for order in [Order::GloballyFirst, Order::GloballyLast] {
        let TwoApps { mut s, d, other, .. } = two_apps();
        let mut raises = capture_raises(&mut s);

        activate(&mut s, 2, other, order);

        assert_eq!(d, s.reactor.contexts.active(), "{order:?}");
        let requests = raise_requests(&mut raises);
        assert_eq!(1, requests.len(), "{order:?}: {requests:?}");
        assert_eq!(Some(other), requests[0].1);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![(other, screen())], s.tiles());
        assert_eq!(screen(), s.frame(other));
        assert_eq!(vec![wid(1)], s.parked());
        assert_eq!(Some(other), s.reactor.layout.selected_window(space()));
    }
}

/// R24, R19. A window in two contexts takes focus from outside both. The
/// switch goes to the one used most recently.
#[test]
fn r24_the_switch_goes_to_the_most_recently_used_context_of_the_window() {
    let TwoApps { mut s, d, other, .. } = two_apps();
    let e = s.create("E", &[other]);
    s.switch(e);
    let c = s.create("C2", &[wid(1)]);
    s.switch(c);
    end_raises(&mut s);
    assert_eq!(vec![other], s.parked());
    let _raises = capture_raises(&mut s);

    activate(&mut s, 2, other, Order::GloballyFirst);

    assert_eq!(e, s.reactor.contexts.active());
    assert_ne!(d, s.reactor.contexts.active());
}

/// R25. After the switch that focus from outside started, an activation of
/// another app, as parking the focused window can cause, doesn't switch
/// until the switch's raise sequence ends. The activation that arrived
/// during the wait applies then, and the next one switches.
#[test]
fn r25_an_activation_before_the_switchs_raise_ends_does_not_switch() {
    for end in ["completed", "failed", "timed out"] {
        let TwoApps { mut s, c, d, other } = two_apps();
        let mut raises = capture_raises(&mut s);
        activate(&mut s, 2, other, Order::GloballyFirst);
        assert_eq!(d, s.reactor.contexts.active());
        let [(sequence_id, _)] = raise_requests(&mut raises)[..] else {
            panic!()
        };
        s.apps.simulate_until_quiet(&mut s.reactor);

        activate(&mut s, 1, wid(1), Order::GloballyLast);
        assert_eq!(d, s.reactor.contexts.active(), "{end}");
        assert!(raise_requests(&mut raises).is_empty(), "{end}");

        // The focusing raise goes out, and its own end follows.
        s.reactor.handle_event(Event::RaiseFocusSent { sequence_id });
        s.reactor.handle_event(match end {
            "completed" => Event::RaiseCompleted { window_id: other, sequence_id },
            "failed" => Event::RaiseRequestFailed {
                windows: vec![other],
                sequence_id,
                quiet: Quiet::No,
            },
            _ => Event::RaiseTimeout { sequence_id },
        });

        // The activation that arrived during the wait applies now.
        assert_eq!(c, s.reactor.contexts.active(), "{end}");
        end_raises(&mut s);
        // The return switch also waits for its parking write.
        s.apps.simulate_until_quiet(&mut s.reactor);
        activate(&mut s, 2, other, Order::GloballyFirst);
        assert_eq!(d, s.reactor.contexts.active(), "{end}");
    }
}

/// R25. A switch that raises nothing, because the window to focus has the
/// focus already, waits for the echo of every window it parked.
#[test]
fn r25_a_switch_that_raises_nothing_waits_for_the_echoes_of_its_parking() {
    let TwoApps { mut s, c, d, other } = two_apps();
    s.switch(ContextKey::Everything);
    end_raises(&mut s);
    // App 1's window has the focus, quietly.
    s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
    s.reactor.handle_event(Event::ApplicationActivated(1, Quiet::Yes));
    s.reactor
        .handle_event(Event::ApplicationMainWindowChanged(1, Some(wid(1)), Quiet::Yes));
    let mut raises = capture_raises(&mut s);

    s.command(c);
    assert!(raise_requests(&mut raises).iter().all(|(_, focus)| focus.is_none()));
    let parking = s.apps.requests();
    assert_eq!(1, frame_writes(&parking, other).len());
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(c, s.reactor.contexts.active(), "the echo hasn't arrived");

    answer(&mut s, parking);
    s.apps.simulate_until_quiet(&mut s.reactor);
    activate(&mut s, 2, other, Order::GloballyLast);
    assert_eq!(d, s.reactor.contexts.active());
}

/// R24. Focus on a window the layout doesn't track, such as a floating
/// panel, or on one of Sugarglider's own windows, doesn't switch.
#[test]
fn r24_focus_on_an_untracked_or_own_window_does_not_switch() {
    let TwoApps { mut s, c, .. } = two_apps();
    let own_pid = std::process::id() as i32;
    let own = launch(
        &mut s,
        own_pid,
        test_app_info(own_pid),
        vec![window_at(41, 400.)],
        &[wid(1)],
    )[0];
    let panel = WindowId::new(3, 1);
    let mut events = s.apps.make_app(3, vec![window_at(31, 300.)]);
    for event in &mut events {
        if let Event::WindowsOnScreenUpdated { on_screen, .. } = event {
            *on_screen = WindowsOnScreen::new(vec![
                on_screen_info(&s, wid(1)),
                on_screen_info(&s, own),
                WindowServerInfo { layer: 3, ..on_screen.info[0] },
            ]);
        }
    }
    s.reactor.handle_events(events);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert!(s.reactor.contexts.is_unsorted(panel));
    assert!(s.reactor.contexts.is_unsorted(own));
    assert!(s.parked().iter().all(|&parked| parked != panel && parked != own));
    let mut raises = capture_raises(&mut s);

    activate(&mut s, 3, panel, Order::GloballyFirst);
    assert_eq!(c, s.reactor.contexts.active());
    activate(&mut s, own_pid, own, Order::GloballyLast);
    assert_eq!(c, s.reactor.contexts.active());
    assert!(raise_requests(&mut raises).is_empty());
}

/// R24, R20, R38. Launching an app never switches to Unsorted, whatever
/// order the launch events arrive in: the new window's membership is
/// decided first, and focus on a window the reactor hasn't seen waits for
/// it.
#[test]
fn r24_launching_an_app_never_switches_to_unsorted() {
    for order in [
        "with its window",
        "window discovered later",
        "window created later",
    ] {
        let TwoApps { mut s, c, .. } = two_apps();
        let mut raises = capture_raises(&mut s);
        let launched = WindowId::new(3, 1);
        let window = window_at(31, 300.);
        s.apps.windows.insert(
            launched,
            crate::actor::reactor::testing::WindowState {
                frame: window.frame,
                ..Default::default()
            },
        );
        let handle_launch = |s: &mut Setup, windows: Vec<(WindowId, WindowInfo)>| {
            let mut events = s.apps.make_app_with_opts(3, vec![], Some(launched), true);
            for event in &mut events {
                if let Event::ApplicationLaunched { visible_windows, .. } = event {
                    *visible_windows = windows.clone();
                }
            }
            s.reactor.handle_events(events);
        };
        s.reactor.handle_event(Event::ApplicationGloballyActivated(3));
        match order {
            "with its window" => handle_launch(&mut s, vec![(launched, window)]),
            "window discovered later" => {
                s.reactor.handle_event(Event::ApplicationActivated(3, Quiet::No));
                handle_launch(&mut s, vec![]);
                s.reactor.handle_event(Event::WindowsDiscovered {
                    pid: 3,
                    new: vec![(launched, window)],
                    known_visible: vec![],
                });
            }
            _ => {
                handle_launch(&mut s, vec![]);
                s.reactor.handle_event(Event::ApplicationMainWindowChanged(
                    3,
                    Some(launched),
                    Quiet::No,
                ));
                s.reactor.handle_event(Event::WindowCreated(launched, window, MouseState::Up));
            }
        }
        report_visible(&mut s, &[wid(1), launched]);

        assert_eq!(c, s.reactor.contexts.active(), "{order}");
        assert!(s.reactor.contexts.is_member(c, launched), "{order}");
        assert!(!s.parked().contains(&launched), "{order}");
        let focused: Vec<WindowId> =
            raise_requests(&mut raises).into_iter().filter_map(|(_, focus)| focus).collect();
        assert!(
            focused.iter().all(|&wid| wid == launched),
            "{order}: {focused:?}"
        );
    }
}

/// R24, R21, R38. An app launches with a window that rejoins D by its
/// record while C is active, and the window has the focus: Sugarglider
/// switches to D once the window is seen, even when the focus was reported
/// first.
#[test]
fn r24_focus_that_waits_for_a_window_switches_when_the_window_rejoins_another_context() {
    let TwoApps { mut s, d, .. } = two_apps();
    let old = WindowId::new(3, 9);
    let desc = WindowDesc {
        wid: old,
        bundle_id: Some("com.testapp3".into()),
        app_name: Some("TestApp3".into()),
        title: "Window1".into(),
        window_server_id: None,
    };
    s.reactor.contexts.add_window(id_of(d), &desc).unwrap();
    s.reactor.contexts.window_closed(old);
    s.reactor.contexts.app_terminated(3);
    let mut raises = capture_raises(&mut s);
    let launched = WindowId::new(3, 1);
    let window = window_at(31, 300.);
    s.apps.windows.insert(
        launched,
        crate::actor::reactor::testing::WindowState {
            frame: window.frame,
            ..Default::default()
        },
    );
    s.reactor.handle_events(s.apps.make_app_with_opts(3, vec![], None, true));
    s.reactor.handle_event(Event::ApplicationGloballyActivated(3));
    s.reactor
        .handle_event(Event::ApplicationMainWindowChanged(3, Some(launched), Quiet::No));
    assert_ne!(d, s.reactor.contexts.active());

    s.reactor.handle_event(Event::WindowCreated(launched, window, MouseState::Up));
    let on_screen = on_screen(&s, &[wid(1), launched]);
    s.reactor
        .handle_event(Event::WindowsOnScreenUpdated { pid: Some(3), on_screen });

    assert_eq!(d, s.reactor.contexts.active());
    assert!(s.reactor.contexts.is_member(d, launched));
    assert_eq!(
        vec![Some(launched)],
        raise_requests(&mut raises)
            .into_iter()
            .map(|(_, focus)| focus)
            .collect::<Vec<_>>()
    );
}

/// App 1 has window 1 in C and window 2 in D, like a browser with a window
/// in each. C is active, so window 2 is parked, and it is still app 1's
/// main window.
fn one_app_in_two_contexts() -> (Setup, ContextKey, ContextKey) {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[wid(2)]);
    s.switch(d);
    s.switch(c);
    end_raises(&mut s);
    assert_eq!(vec![wid(2)], s.parked());
    (s, c, d)
}

/// R40. The user activates an app whose main window is parked, and the app
/// has a visible member of the active context. Sugarglider raises that
/// member and doesn't switch, whichever order the activation events arrive
/// in.
#[test]
fn r40_activating_an_app_whose_main_window_is_parked_raises_its_member() {
    for order in [Order::GloballyFirst, Order::GloballyLast] {
        let (mut s, c, _) = one_app_in_two_contexts();
        let mut raises = capture_raises(&mut s);

        activate(&mut s, 1, wid(2), order);

        assert_eq!(c, s.reactor.contexts.active(), "{order:?}");
        let focused: Vec<Option<WindowId>> =
            raise_requests(&mut raises).into_iter().map(|(_, focus)| focus).collect();
        assert_eq!(vec![Some(wid(1))], focused, "{order:?}");
        assert_eq!(vec![wid(2)], s.parked());
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(vec![(wid(1), screen())], s.tiles());
    }
}

/// R24, R40, R36. App 1 is frontmost on window 1, which is in C. The user
/// presses ⌘` and window 2, parked because it is only in D, becomes the main
/// window. A window focus change inside the frontmost app is no app
/// activation, so R40 doesn't raise window 1 again: R24 applies and
/// Sugarglider switches to D, which shows window 2.
#[test]
fn r24_a_main_window_change_inside_the_frontmost_app_switches_to_its_context() {
    let (mut s, _c, d) = one_app_in_two_contexts();
    s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
    s.reactor.handle_event(Event::ApplicationActivated(1, Quiet::Yes));
    s.reactor
        .handle_event(Event::ApplicationMainWindowChanged(1, Some(wid(1)), Quiet::Yes));
    assert_eq!(Some(wid(1)), s.reactor.main_window());
    let mut raises = capture_raises(&mut s);

    s.reactor
        .handle_event(Event::ApplicationMainWindowChanged(1, Some(wid(2)), Quiet::No));

    assert_eq!(d, s.reactor.contexts.active());
    let focused: Vec<Option<WindowId>> =
        raise_requests(&mut raises).into_iter().map(|(_, focus)| focus).collect();
    assert_eq!(vec![Some(wid(2))], focused);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(vec![(wid(2), screen())], s.tiles());
    assert_eq!(vec![wid(1)], s.parked());
}

/// R40. With several visible members, the most recently focused one is
/// raised.
#[test]
fn r40_the_most_recently_focused_member_is_raised() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1), wid(2)]);
    let d = s.create("D", &[wid(3)]);
    s.switch(d);
    s.switch(c);
    end_raises(&mut s);
    s.reactor.contexts.window_focused(wid(2));
    s.reactor.contexts.window_focused(wid(1));
    let mut raises = capture_raises(&mut s);

    activate(&mut s, 1, wid(3), Order::GloballyFirst);

    assert_eq!(c, s.reactor.contexts.active());
    let focused: Vec<Option<WindowId>> =
        raise_requests(&mut raises).into_iter().map(|(_, focus)| focus).collect();
    assert_eq!(vec![Some(wid(1))], focused);
}

/// R12 step 6, R25. A switch to a context with no window to focus activates
/// Finder quietly. Finder's activation, and a main window change to its
/// parked window that macOS reports as the user's, don't switch again.
/// Once Finder's activation arrives, focus from outside counts again.
#[test]
fn r12_step_6_a_switch_that_leaves_no_window_to_focus_activates_finder() {
    let TwoApps { mut s, c, .. } = two_apps();
    let finder_info = AppInfo {
        bundle_id: Some("com.apple.finder".into()),
        localized_name: Some("Finder".into()),
    };
    let finder_window = launch(&mut s, 9, finder_info, vec![window_at(91, 900.)], &[wid(1)])[0];
    assert!(s.reactor.contexts.is_member(c, finder_window));
    let empty = s.create("Empty", &[]);
    let mut raises = capture_raises(&mut s);

    s.command(empty);

    let requests = s.apps.requests();
    let activations: Vec<&Request> = requests
        .iter()
        .filter(|request| matches!(request, Request::Activate(_)))
        .collect();
    assert!(
        matches!(activations[..], [Request::Activate(Quiet::Yes)]),
        "{requests:?}"
    );
    assert!(raise_requests(&mut raises).is_empty());
    answer(&mut s, requests);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(vec![wid(1), WindowId::new(2, 1), finder_window], s.parked());

    s.reactor.handle_event(Event::ApplicationMainWindowChanged(
        9,
        Some(finder_window),
        Quiet::No,
    ));
    s.reactor.handle_event(Event::ApplicationGloballyActivated(9));
    assert_eq!(empty, s.reactor.contexts.active());
    s.reactor.handle_event(Event::ApplicationActivated(9, Quiet::Yes));
    assert_eq!(empty, s.reactor.contexts.active());
    assert!(raise_requests(&mut raises).is_empty());

    activate(&mut s, 1, wid(1), Order::GloballyFirst);
    assert_eq!(c, s.reactor.contexts.active());
}

/// R25. When the events that end a switch never arrive, focus from
/// outside counts again at the first visibility refresh 2 seconds after the
/// switch.
#[test]
fn r25_a_switch_whose_end_never_arrives_stops_waiting_after_2_seconds() {
    let TwoApps { mut s, c, d, other } = two_apps();
    let _raises = capture_raises(&mut s);
    activate(&mut s, 2, other, Order::GloballyFirst);
    assert_eq!(d, s.reactor.contexts.active());
    s.apps.simulate_until_quiet(&mut s.reactor);

    let since = s.reactor.switch_guard.since.unwrap();
    s.reactor.guard_deadline_tick(since + Duration::from_millis(1999));
    activate(&mut s, 1, wid(1), Order::GloballyFirst);
    assert_eq!(d, s.reactor.contexts.active());

    s.reactor.guard_deadline_tick(since + Duration::from_secs(2));
    activate(&mut s, 1, wid(1), Order::GloballyLast);
    assert_eq!(c, s.reactor.contexts.active());
}

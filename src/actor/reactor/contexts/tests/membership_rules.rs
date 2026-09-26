// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reactor tests for the membership rules of M5b, taken from the spec: windows
//! that appear during a switch or before startup completes, relaunches whose
//! windows arrive together, every order of a quit, windows that come back,
//! pinning, title changes, the membership commands on windows and targets
//! they can't act on, and the paths with contexts off.

use test_log::test;

use super::*;
use crate::actor::reactor::testing::WindowState;
use crate::sys::app::AppInfo;

/// A window of an app other than app 1, titled `title`, with the window
/// server id `sys_id`, at `x` on the screen.
fn titled(title: &str, sys_id: u32, x: f64) -> WindowInfo {
    WindowInfo {
        title: title.to_string().into(),
        sys_id: Some(WindowServerId::new(sys_id)),
        frame: rect(x, 100., 50., 50.),
        ..make_window(1)
    }
}

/// App `pid`, which `info` describes, registers with `windows`, and the
/// window server lists every window of every app. Returns the app's windows.
fn launch(s: &mut Setup, pid: i32, info: AppInfo, windows: Vec<WindowInfo>) -> Vec<WindowId> {
    let count = windows.len() as u32;
    let events = s.apps.make_app_with_info(pid, info, windows, None, false);
    s.reactor.handle_events(events);
    let all: Vec<WindowId> = s.apps.windows.keys().copied().collect();
    report_visible(s, &all);
    (1..=count).map(|idx| WindowId::new(pid, idx)).collect()
}

/// App `wid.pid` opens a window, which reaches the reactor as a new window
/// does: `WindowCreated`, then the window server's list, which names the
/// windows in `listed` and the new one, then `WindowBecameVisible`. The apps
/// don't answer yet.
fn arrive(s: &mut Setup, wid: WindowId, info: WindowInfo, listed: &[WindowId]) {
    s.apps.windows.insert(
        wid,
        WindowState {
            frame: info.frame,
            ..Default::default()
        },
    );
    s.reactor.handle_event(Event::WindowCreated(wid, info, MouseState::Up));
    let mut listed = listed.to_vec();
    listed.push(wid);
    let on_screen = on_screen(s, &listed);
    s.reactor
        .handle_event(Event::WindowsOnScreenUpdated { pid: Some(wid.pid), on_screen });
    s.reactor.handle_event(Event::WindowBecameVisible(wid));
}

/// Like [`arrive`], and the apps answer.
fn open_window(s: &mut Setup, wid: WindowId, info: WindowInfo, listed: &[WindowId]) {
    arrive(s, wid, info, listed);
    s.apps.simulate_until_quiet(&mut s.reactor);
}

/// Adds to the context an empty record of a window of app `app` titled
/// `title`, as an earlier run of the app with pid `gone.pid` leaves it.
fn empty_record(s: &mut Setup, key: ContextKey, gone: WindowId, app: i32, title: &str) {
    let desc = WindowDesc {
        wid: gone,
        bundle_id: Some(format!("com.testapp{app}")),
        app_name: Some(format!("TestApp{app}")),
        title: title.into(),
        window_server_id: None,
    };
    s.reactor.contexts.add_window(id_of(key), &desc).unwrap();
    s.reactor.contexts.window_closed(gone);
    s.reactor.contexts.app_terminated(gone.pid);
}

/// The title of each record of the context and whether it has a window.
fn records(s: &Setup, key: ContextKey) -> Vec<(String, RecordLink)> {
    let members = &s.reactor.contexts.get(id_of(key)).unwrap().members;
    members.iter().map(|m| (m.title.clone(), m.link)).collect()
}

fn record(title: &str, link: RecordLink) -> (String, RecordLink) {
    (title.to_string(), link)
}

/// The titles of the records that `contexts.json` holds for the context.
fn saved_members(s: &Setup, key: ContextKey) -> Vec<String> {
    match ContextsStore::new(s.dir.path().join("contexts.json")).load(SystemTime::now()) {
        Loaded::Read { contexts, .. } => contexts
            .get(id_of(key))
            .unwrap()
            .members
            .iter()
            .map(|m| m.title.clone())
            .collect(),
        other => panic!("{other:?}"),
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

fn run(s: &mut Setup, command: ContextCommand) {
    s.reactor.handle_event(Event::Command(Command::Context(command)));
}

/// The frame writes in `requests`, in order.
fn writes_in(requests: &[Request]) -> Vec<(WindowId, CGRect)> {
    requests
        .iter()
        .filter_map(|request| match request {
            Request::SetWindowFrame(wid, frame, _) => Some((*wid, *frame)),
            _ => None,
        })
        .collect()
}

/// The app moves the window to `frame` on its own, after the last write to it.
fn move_by_app(s: &mut Setup, wid: WindowId, frame: CGRect) {
    let txid = s.reactor.windows[&wid].last_sent_txid;
    s.apps.windows.get_mut(&wid).unwrap().frame = frame;
    s.reactor.handle_event(Event::WindowFrameChanged(
        wid,
        frame,
        txid,
        Requested(false),
        None,
    ));
}

fn halves(left: WindowId, right: WindowId) -> Vec<(WindowId, CGRect)> {
    let mut tiles = vec![
        (left, rect(0., 0., 600., 1000.)),
        (right, rect(600., 0., 600., 1000.)),
    ];
    tiles.sort_by_key(|(wid, _)| *wid);
    tiles
}

/// R20, R12. A window appears while a switch from C to D waits for its apps
/// to answer. It joins D, the context the switch made active, and not C, and
/// it shows next to D's member.
#[test]
fn r20_a_window_that_appears_during_a_switch_joins_the_switchs_target() {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[wid(2)]);
    s.switch(c);
    assert_eq!(vec![wid(2)], s.parked());

    s.command(d);
    let switching = s.apps.requests();
    let info = WindowInfo {
        frame: rect(700., 100., 50., 50.),
        ..make_window(3)
    };
    arrive(&mut s, wid(3), info, &[wid(1), wid(2)]);
    answer(&mut s, switching);
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert_eq!(vec![id_of(d)], s.reactor.contexts.contexts_of(wid(3)));
    assert_eq!(vec![wid(1)], s.parked());
    let tiles = halves(wid(2), wid(3));
    assert_eq!(tiles, s.tiles());
    assert_eq!(tiles, s.frames(&[wid(2), wid(3)]));
    assert_eq!(corner(screen().size), s.frame(wid(1)));
}

/// R38, R20, R13. Before startup completes, the user switches to C, and a
/// window appears while the switch waits for its app. The reactor first sees
/// the window before startup completes, so it was open before Sugarglider
/// started: it stays unsorted instead of joining C, and it is parked when
/// startup completes. C's member keeps its tile.
#[test]
fn r38_a_window_first_seen_before_startup_completes_stays_unsorted_during_a_switch() {
    let mut s = Setup::on(vec![screen()], vec![Some(space())]);
    s.reactor.handle_events(s.apps.make_app(1, make_windows(2)));
    s.apps.simulate_until_quiet(&mut s.reactor);
    let c = s.create("C", &[wid(1)]);

    s.command(c);
    let switching = s.apps.requests();
    let info = WindowInfo {
        frame: rect(700., 100., 50., 50.),
        ..make_window(3)
    };
    arrive(&mut s, wid(3), info, &[wid(1), wid(2)]);
    answer(&mut s, switching);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert!(s.reactor.contexts.is_unsorted(wid(3)));
    assert_eq!(vec![wid(2)], s.parked());

    s.reactor.handle_event(Event::StartupComplete);
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert!(s.reactor.contexts.is_unsorted(wid(3)));
    assert_eq!(vec![wid(2), wid(3)], s.parked());
    assert_eq!(corner(CGSize::new(50., 50.)), s.frame(wid(3)));
    assert_eq!(vec![(wid(1), screen())], s.tiles());
    assert_eq!(screen(), s.frame(wid(1)));
}

/// R21, R22, R20. App 2 runs again while C is active, and its three windows
/// arrive together. "Inbox" matches C's empty record exactly and rejoins C.
/// "Inbox — 3 unread" is similar to that record, but the exact match takes
/// the record first, so the window matches nothing and joins C as a new
/// window. "Quarterly report (edited)" is similar to D's record, rejoins D and
/// not C, and is parked. The result is the same in either order of the
/// windows.
#[test]
fn r21_r22_windows_that_arrive_together_rejoin_by_exact_and_similar_titles_in_any_order() {
    for reversed in [false, true] {
        let mut s = Setup::new(1);
        let c = s.create("C", &[wid(1)]);
        let d = s.create("D", &[]);
        empty_record(&mut s, c, WindowId::new(90, 1), 2, "Inbox");
        empty_record(&mut s, d, WindowId::new(91, 1), 2, "Quarterly report");
        s.switch(c);

        let mut windows = vec![
            titled("Inbox — 3 unread", 51, 700.),
            titled("Inbox", 52, 800.),
            titled("Quarterly report (edited)", 53, 900.),
        ];
        if reversed {
            windows.reverse();
        }
        let wid_of = |title: &str| {
            let idx = windows.iter().position(|w| w.title.expose_secret() == title).unwrap();
            WindowId::new(5, idx as u32 + 1)
        };
        let (similar, exact, report) = (
            wid_of("Inbox — 3 unread"),
            wid_of("Inbox"),
            wid_of("Quarterly report (edited)"),
        );
        let mut launch = s.apps.make_app_with_info(5, test_app_info(2), windows, None, false);
        for event in &mut launch {
            if let Event::WindowsOnScreenUpdated { on_screen, .. } = event {
                on_screen.info.insert(0, on_screen_info(&s, wid(1)));
                on_screen.visible.insert(0, WindowServerId::new(1));
            }
        }
        s.reactor.handle_events(launch);
        s.apps.simulate_until_quiet(&mut s.reactor);

        let what = format!("reversed: {reversed}");
        assert_eq!(
            vec![
                record("Window1", RecordLink::Live(wid(1))),
                record("Inbox", RecordLink::Live(exact)),
                record("Inbox — 3 unread", RecordLink::Live(similar)),
            ],
            records(&s, c),
            "{what}"
        );
        assert_eq!(
            vec![record(
                "Quarterly report (edited)",
                RecordLink::Live(report)
            )],
            records(&s, d),
            "{what}"
        );
        assert_eq!(
            vec!["Window1", "Inbox", "Inbox — 3 unread"],
            saved_members(&s, c),
            "{what}"
        );
        assert_eq!(vec!["Quarterly report (edited)"], saved_members(&s, d));
        assert_eq!(vec![report], s.parked(), "{what}");
        assert_eq!(corner(CGSize::new(50., 50.)), s.frame(report));
        assert_eq!(
            vec![JournalEntry {
                pid: 5,
                bundle_id: Some("com.testapp2".into()),
                window_server_id: WindowServerId::new(53),
                title: "Quarterly report (edited)".into(),
                frame: rect(900., 100., 50., 50.).into(),
            }],
            s.journal_on_disk(),
            "{what}"
        );
        let tiles = s.tiles();
        let mut tiled = vec![wid(1), exact, similar];
        tiled.sort();
        assert_eq!(tiles, s.frames(&tiled), "{what}");
        assert_eq!((wid(1), rect(0., 0., 400., 1000.)), tiles[0], "{what}");
        let mut rest: Vec<(WindowId, CGRect)> = tiles[1..].to_vec();
        rest.sort_by(|a, b| a.1.origin.x.total_cmp(&b.1.origin.x));
        let rest_frames: Vec<CGRect> = rest.iter().map(|&(_, frame)| frame).collect();
        assert_eq!(
            vec![rect(400., 0., 400., 1000.), rect(800., 0., 400., 1000.)],
            rest_frames,
            "{what}"
        );
        let mut rest_wids: Vec<WindowId> = rest.iter().map(|&(wid, _)| wid).collect();
        rest_wids.sort();
        let mut expected = vec![exact, similar];
        expected.sort();
        assert_eq!(expected, rest_wids, "{what}");

        s.switch(d);
        assert_eq!(vec![(report, screen())], s.tiles(), "{what}");
        assert_eq!(screen(), s.frame(report));
        let mut parked = vec![wid(1), exact, similar];
        parked.sort();
        assert_eq!(parked, s.parked(), "{what}");
    }
}

/// R22, R20. C holds an empty record of app 2 with a blank title and no
/// window server id. App 2 runs again while D is active, with a window whose
/// title is blank too. Steps 2 and 3 never take a blank title, so the window
/// matches nothing: it joins D and shows there, and C's record stays empty.
#[test]
fn r22_a_relaunched_window_with_a_blank_title_matches_no_blank_record() {
    for title in ["", "  "] {
        let mut s = Setup::new(1);
        let c = s.create("C", &[wid(1)]);
        let d = s.create("D", &[wid(1)]);
        empty_record(&mut s, c, WindowId::new(90, 1), 2, title);
        s.switch(d);

        let blank = launch(&mut s, 5, test_app_info(2), vec![titled(title, 51, 700.)])[0];

        assert_eq!(
            vec![
                record("Window1", RecordLink::Live(wid(1))),
                record(title, RecordLink::Empty),
            ],
            records(&s, c),
            "{title:?}"
        );
        assert_eq!(
            vec![id_of(d)],
            s.reactor.contexts.contexts_of(blank),
            "{title:?}"
        );
        assert!(s.parked().is_empty(), "{title:?}");
        let tiles = halves(wid(1), blank);
        assert_eq!(tiles, s.tiles(), "{title:?}");
        assert_eq!(tiles, s.frames(&[wid(1), blank]), "{title:?}");
    }
}

/// App 2 with window "Doc A" in C and window "Doc B" in C and D. App 1's
/// window 1 is in both. C is active. Returns the setup, C, D, and app 2's
/// windows.
fn quitting_app() -> (Setup, ContextKey, ContextKey, WindowId, WindowId) {
    let mut s = Setup::new(1);
    let windows = vec![titled("Doc A", 21, 700.), titled("Doc B", 22, 900.)];
    let [a, b] = launch(&mut s, 2, test_app_info(2), windows)[..] else {
        panic!()
    };
    let c = s.create("C", &[wid(1), a, b]);
    let d = s.create("D", &[wid(1), b]);
    s.switch(c);
    assert!(s.parked().is_empty());
    (s, c, d, a, b)
}

/// One event of app 2's quit. The apps answer the writes it causes before the
/// next event, as they answer them before they report the next close.
fn quit_step(s: &mut Setup, step: &str, a: WindowId, b: WindowId) {
    match step {
        "destroy a" => s.close(a),
        "destroy b" => s.close(b),
        "terminated" => s.reactor.handle_event(Event::ApplicationTerminated(2)),
        "thread" => {
            s.apps.windows.retain(|wid, _| wid.pid != 2);
            s.reactor.handle_event(Event::ApplicationThreadTerminated(2));
        }
        _ => panic!("{step}"),
    }
    s.apps.simulate_until_quiet(&mut s.reactor);
}

/// R23, R21. App 2 quits with ⌘Q, and macOS reports its two windows
/// destroyed before, between, or after `ApplicationTerminated`, before or
/// after its thread ends, or not at all. In every order the records of both
/// windows stay, empty, with their titles, and `contexts.json` keeps them.
/// When the app runs again, each window rejoins every context that held it.
#[test]
fn r23_every_order_of_a_quit_keeps_the_records_of_every_window() {
    let orders: [&[&str]; 6] = [
        &["destroy a", "destroy b", "terminated", "thread"],
        &["terminated", "destroy a", "destroy b", "thread"],
        &["destroy a", "terminated", "destroy b", "thread"],
        &["destroy b", "destroy a", "thread", "terminated"],
        &["terminated", "thread"],
        &["thread", "terminated"],
    ];
    for order in orders {
        let (mut s, c, d, a, b) = quitting_app();
        for step in order {
            quit_step(&mut s, step, a, b);
        }

        assert_eq!(
            vec![
                record("Window1", RecordLink::Live(wid(1))),
                record("Doc A", RecordLink::Empty),
                record("Doc B", RecordLink::Empty),
            ],
            records(&s, c),
            "{order:?}"
        );
        assert_eq!(
            vec![
                record("Window1", RecordLink::Live(wid(1))),
                record("Doc B", RecordLink::Empty),
            ],
            records(&s, d),
            "{order:?}"
        );
        assert_eq!(
            vec!["Window1", "Doc A", "Doc B"],
            saved_members(&s, c),
            "{order:?}"
        );
        assert_eq!(vec!["Window1", "Doc B"], saved_members(&s, d), "{order:?}");

        let windows = vec![titled("Doc B", 52, 900.), titled("Doc A", 51, 700.)];
        let [b2, a2] = launch(&mut s, 5, test_app_info(2), windows)[..] else {
            panic!()
        };
        assert_eq!(
            vec![
                record("Window1", RecordLink::Live(wid(1))),
                record("Doc A", RecordLink::Live(a2)),
                record("Doc B", RecordLink::Live(b2)),
            ],
            records(&s, c),
            "{order:?}"
        );
        assert_eq!(
            vec![
                record("Window1", RecordLink::Live(wid(1))),
                record("Doc B", RecordLink::Live(b2)),
            ],
            records(&s, d),
            "{order:?}"
        );
        assert!(s.parked().is_empty(), "{order:?}");
        s.switch(d);
        assert_eq!(vec![a2], s.parked(), "{order:?}");
        assert_eq!(halves(wid(1), b2), s.tiles(), "{order:?}");
    }
}

/// R23, Q4. A window server list inside the quit gap is no longer a signal
/// that the app still runs, so the records of a window that closed stay
/// pending whatever the list names, and stay when the app terminates.
#[test]
fn r23_a_window_list_between_the_closes_of_a_quit_keeps_the_first_windows_records() {
    for listed in [
        "Doc B",
        "other apps",
        "the closed window",
        "Doc B, terminated",
    ] {
        let (mut s, c, d, a, b) = quitting_app();
        let closed = on_screen_info(&s, a);
        if listed == "Doc B, terminated" {
            quit_step(&mut s, "terminated", a, b);
        }
        quit_step(&mut s, "destroy a", a, b);
        s.reactor
            .handle_event(Event::ApplicationMainWindowChanged(2, Some(b), Quiet::No));
        let on_screen = match listed {
            "other apps" => on_screen(&s, &[wid(1)]),
            "the closed window" => WindowsOnScreen::new(vec![on_screen_info(&s, wid(1)), closed]),
            _ => on_screen(&s, &[wid(1), b]),
        };
        s.reactor
            .handle_event(Event::WindowsOnScreenUpdated { pid: Some(2), on_screen });
        s.apps.simulate_until_quiet(&mut s.reactor);

        if listed != "Doc B, terminated" {
            assert_eq!(
                vec![
                    record("Window1", RecordLink::Live(wid(1))),
                    record("Doc A", RecordLink::Pending(a)),
                    record("Doc B", RecordLink::Live(b)),
                ],
                records(&s, c),
                "{listed}"
            );
        }
        quit_step(&mut s, "destroy b", a, b);
        quit_step(&mut s, "terminated", a, b);
        quit_step(&mut s, "thread", a, b);

        assert_eq!(
            vec![
                record("Window1", RecordLink::Live(wid(1))),
                record("Doc A", RecordLink::Empty),
                record("Doc B", RecordLink::Empty),
            ],
            records(&s, c),
            "{listed}"
        );
        assert_eq!(
            vec![
                record("Window1", RecordLink::Live(wid(1))),
                record("Doc B", RecordLink::Empty),
            ],
            records(&s, d),
            "{listed}"
        );
        let saved: Vec<String> = vec![
            "Window1".to_string(),
            "Doc A".to_string(),
            "Doc B".to_string(),
        ];
        assert_eq!(saved, saved_members(&s, c), "{listed}");
    }
}

/// R23. `ApplicationTerminated` stops a closed window's records being
/// pending at once, before its own destroyed event arrives, so they keep
/// their titles and match the windows of a relaunch.
#[test]
fn r23_an_apps_records_stop_being_pending_when_it_terminates() {
    let (mut s, c, _d, a, b) = quitting_app();
    s.close(a);
    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Doc A", RecordLink::Pending(a)),
            record("Doc B", RecordLink::Live(b)),
        ],
        records(&s, c)
    );

    quit_step(&mut s, "terminated", a, b);
    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Doc A", RecordLink::Empty),
            record("Doc B", RecordLink::Empty),
        ],
        records(&s, c)
    );

    // Neither a window server list nor the destroyed window changes them.
    report_visible(&mut s, &[wid(1), b]);
    quit_step(&mut s, "destroy b", a, b);
    quit_step(&mut s, "thread", a, b);
    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Doc A", RecordLink::Empty),
            record("Doc B", RecordLink::Empty),
        ],
        records(&s, c)
    );

    let windows = vec![titled("Doc B", 52, 900.), titled("Doc A", 51, 700.)];
    let [b2, a2] = launch(&mut s, 5, test_app_info(2), windows)[..] else {
        panic!()
    };
    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Doc A", RecordLink::Live(a2)),
            record("Doc B", RecordLink::Live(b2)),
        ],
        records(&s, c)
    );
}

/// R23. A single-window app stays running after the user closes its window
/// with ⌘W. Nothing shows that it still runs: window server lists name only
/// other apps' windows, Sugarglider's own quiet activation doesn't count, and
/// neither does a main window change. A switch leaves the record alone. The
/// record stays pending, then stays when the app quits, and the window rejoins
/// C when the app runs again.
#[test]
fn r23_a_single_window_app_that_stays_running_after_cmd_w_keeps_its_record() {
    let mut s = Setup::new(1);
    let notes = launch(&mut s, 2, test_app_info(2), vec![titled("Notes", 21, 700.)])[0];
    let c = s.create("C", &[wid(1), notes]);
    let d = s.create("D", &[wid(1)]);
    s.switch(c);

    s.close(notes);
    report_visible(&mut s, &[wid(1)]);
    s.reactor.handle_event(Event::ApplicationActivated(2, Quiet::Yes));
    s.reactor.handle_event(Event::ApplicationMainWindowChanged(2, None, Quiet::No));
    s.switch(d);
    report_visible(&mut s, &[wid(1)]);

    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Notes", RecordLink::Pending(notes)),
        ],
        records(&s, c)
    );
    s.reactor.handle_event(Event::ApplicationTerminated(2));
    s.reactor.handle_event(Event::ApplicationThreadTerminated(2));
    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Notes", RecordLink::Empty),
        ],
        records(&s, c)
    );
    assert_eq!(vec!["Window1", "Notes"], saved_members(&s, c));

    let again = launch(&mut s, 6, test_app_info(2), vec![titled("Notes", 61, 700.)])[0];
    assert_eq!(vec![id_of(c)], s.reactor.contexts.contexts_of(again));
    assert_eq!(vec![again], s.parked());
    assert_eq!(vec![(wid(1), screen())], s.tiles());
}

/// R23, R20. App 2's window "Doc", a member of C, closes while D is active.
/// The app then opens another window titled "Doc". That shows the app still
/// runs, so the closed window's record goes. The new window doesn't take it:
/// it joins D, the active context, and shows there.
#[test]
fn r23_a_new_window_of_an_app_whose_window_closed_doesnt_take_the_closed_record() {
    let mut s = Setup::new(1);
    let doc = launch(&mut s, 2, test_app_info(2), vec![titled("Doc", 21, 700.)])[0];
    let c = s.create("C", &[wid(1), doc]);
    let d = s.create("D", &[wid(1)]);
    s.switch(c);
    s.switch(d);
    assert_eq!(vec![doc], s.parked());
    s.close(doc);
    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Doc", RecordLink::Pending(doc)),
        ],
        records(&s, c)
    );

    let again = WindowId::new(2, 2);
    open_window(&mut s, again, titled("Doc", 23, 700.), &[wid(1)]);

    assert_eq!(vec![record("Window1", RecordLink::Live(wid(1)))], records(&s, c));
    assert_eq!(vec![id_of(d)], s.reactor.contexts.contexts_of(again));
    assert!(s.parked().is_empty());
    let tiles = halves(wid(1), again);
    assert_eq!(tiles, s.tiles());
    assert_eq!(tiles, s.frames(&[wid(1), again]));
    assert_eq!(vec!["Window1"], saved_members(&s, c));
    assert_eq!(vec!["Window1", "Doc"], saved_members(&s, d));
}

/// C holds windows 1 and 2 and is active. Window 3 is minimized.
fn c_with_window_3_minimized() -> (Setup, ContextKey) {
    let mut s = Setup::new(3);
    report_visible(&mut s, &[wid(1), wid(2)]);
    let c = s.create("C", &[wid(1), wid(2)]);
    s.switch(c);
    assert!(s.parked().is_empty());
    (s, c)
}

/// App 1 reports windows 1 to 3 visible, as when window 3 is unminimized.
fn unminimize_window_3(s: &mut Setup) {
    let on_screen = on_screen(s, &[wid(1), wid(2), wid(3)]);
    s.reactor
        .handle_event(Event::WindowsOnScreenUpdated { pid: Some(1), on_screen });
}

/// R39, R30, L4. Window 3, which isn't in C, is unminimized while the journal
/// can't be written. It isn't parked, and it gets no tile. Once the journal
/// can be written, the next window list parks it, with its entry written
/// first.
#[test]
fn r39_r30_a_window_that_becomes_visible_while_the_journal_fails_is_parked_later() {
    let (mut s, _) = c_with_window_3_minimized();
    let frame = s.frame(wid(3));
    let in_c = halves(wid(1), wid(2));
    let failing = FailingWrites::start(s.dir.path());

    unminimize_window_3(&mut s);
    let requests = s.apps.requests();
    assert_eq!(Vec::<CGRect>::new(), frame_writes(&requests, wid(3)));
    answer(&mut s, requests);
    s.apps.simulate_until_quiet(&mut s.reactor);
    drop(failing);

    assert!(s.parked().is_empty());
    assert_eq!(frame, s.frame(wid(3)));
    assert_eq!(in_c, s.tiles());
    assert_eq!(in_c, s.frames(&[wid(1), wid(2)]));
    assert!(s.journal_on_disk().is_empty());

    unminimize_window_3(&mut s);
    let requests = s.apps.requests();
    assert_eq!(vec![corner(frame.size)], frame_writes(&requests, wid(3)));
    assert_eq!(vec![entry(3, frame)], s.journal_on_disk());
    answer(&mut s, requests);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(vec![wid(3)], s.parked());
    assert_eq!(corner(frame.size), s.frame(wid(3)));
    assert_eq!(in_c, s.tiles());
}

/// Reports `wids` visible as the window server lists them, with `panel` on
/// layer 1, and asks every app for its windows.
fn report_with_panel(s: &mut Setup, wids: &[WindowId], panel: WindowId) {
    let mut listed = on_screen(s, wids);
    for info in &mut listed.info {
        if Some(info.id) == s.reactor.windows[&panel].window_server_id {
            info.layer = 1;
        }
    }
    s.reactor
        .handle_event(Event::WindowsOnScreenUpdated { pid: None, on_screen: listed });
    s.reactor.update_visible_windows();
    s.apps.simulate_until_quiet(&mut s.reactor);
}

/// R39, R14, R3. Under C, windows that become visible without taking focus
/// are left alone when Sugarglider may not park them or they may show:
/// Sugarglider's own window and a panel on a layer of its own stay where
/// they are, and a pinned window shows and gets a tile.
#[test]
fn r39_r14_own_untracked_and_pinned_windows_that_become_visible_are_not_parked() {
    let mut s = Setup::new(3);
    let own_pid = std::process::id() as i32;
    let own_window = vec![titled("Preferences", 50, 300.)];
    let own = launch(&mut s, own_pid, test_app_info(own_pid), own_window)[0];
    let panel = launch(&mut s, 3, test_app_info(3), vec![titled("Panel", 30, 500.)])[0];
    let all = [wid(1), wid(2), wid(3), own, panel];
    report_with_panel(&mut s, &all, panel);
    s.reactor.contexts.pin(&s.desc(wid(3)));
    report_with_panel(&mut s, &[wid(1), wid(2)], panel);
    let c = s.create("C", &[wid(1), wid(2)]);
    s.switch(c);
    let untouched = [own, panel].map(|wid| (wid, s.frame(wid)));

    report_with_panel(&mut s, &all, panel);

    assert!(s.parked().is_empty());
    assert!(s.journal_on_disk().is_empty());
    assert_eq!(untouched, [own, panel].map(|wid| (wid, s.frame(wid))));
    let thirds = vec![
        (wid(1), rect(0., 0., 400., 1000.)),
        (wid(2), rect(400., 0., 400., 1000.)),
        (wid(3), rect(800., 0., 400., 1000.)),
    ];
    assert_eq!(thirds, s.tiles());
    assert_eq!(thirds, s.frames(&[wid(1), wid(2), wid(3)]));
}

/// R39, Q1. An app moves its parked window back onto the screen ten times.
/// The first move gets exactly one write, which parks the window again. No
/// move gets more than that one write, or a write to another window, and
/// once a write is answered nothing more happens, so Sugarglider never
/// keeps a loop going on its own. The sixth and later moves get no write.
/// The journal keeps the frame from before the first parking, and C's member
/// keeps its tile.
#[test]
fn r39_an_app_that_keeps_moving_its_parked_window_back_gets_at_most_one_write_per_move() {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1)]);
    s.switch(c);
    let parked_at = corner(CGSize::new(600., 1000.));
    assert_eq!(parked_at, s.frame(wid(2)));
    let journal = s.journal_on_disk();
    assert_eq!(vec![entry(2, rect(600., 0., 600., 1000.))], journal);

    for round in 0..10 {
        let moved = rect(100. + 10. * f64::from(round), 200., 600., 700.);
        move_by_app(&mut s, wid(2), moved);
        let requests = s.apps.requests();
        let writes = writes_in(&requests);
        if round < 5 {
            assert_eq!(vec![(wid(2), parked_at)], writes);
        } else {
            assert!(writes.is_empty(), "round {round}: {writes:?}");
        }
        answer(&mut s, requests);
        assert!(s.apps.requests().is_empty(), "round {round}");
    }

    assert_eq!(vec![wid(2)], s.parked());
    assert_eq!(journal, s.journal_on_disk());
    assert_eq!(vec![(wid(1), screen())], s.tiles());
    assert_eq!(screen(), s.frame(wid(1)));
}

/// R39, Q1. A refused routine repark leaves the window where the app put it.
/// The requested echo starts no write loop, and the journal still has the
/// original frame for a later restore.
#[test]
fn r39_a_refused_repark_keeps_the_context_and_starts_no_write_loop() {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1)]);
    s.switch(c);
    let parked_at = corner(CGSize::new(600., 1000.));
    let kept = rect(300., 200., 600., 700.);

    for round in 0..3 {
        move_by_app(&mut s, wid(2), kept);
        let requests = s.apps.requests();
        let writes = writes_in(&requests);
        if round == 0 {
            assert_eq!(vec![(wid(2), parked_at)], writes);
        } else {
            assert!(
                writes.is_empty() || writes == vec![(wid(2), parked_at)],
                "{writes:?}"
            );
        }
        let Some(txid) = requests.iter().find_map(|request| match request {
            Request::SetWindowFrame(wid, _, txid) if *wid == self::wid(2) => Some(*txid),
            _ => None,
        }) else {
            continue;
        };
        s.apps.windows.get_mut(&wid(2)).unwrap().last_seen_txid = txid;
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(2),
            kept,
            txid,
            Requested(true),
            None,
        ));
        assert!(s.apps.requests().is_empty(), "round {round}");
    }

    s.reactor
        .parking_deadline_tick(std::time::Instant::now() + std::time::Duration::from_secs(3));
    assert_eq!(c, s.reactor.contexts.active());
    assert_eq!(vec![wid(2)], s.parked());
    assert_eq!(vec![entry(2, rect(600., 0., 600., 1000.))], s.journal_on_disk());
    assert_eq!(vec![(wid(1), screen())], s.tiles());
}

/// R22. A title change reaches every record of the window: in C, in D, and
/// in the pinned list, and the reactor's window takes the title too. A title
/// change of a window the reactor doesn't know changes nothing.
#[test]
fn r22_a_title_change_updates_the_window_in_every_context_that_holds_it() {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1), wid(2)]);
    let d = s.create("D", &[wid(2)]);
    s.reactor.contexts.pin(&s.desc(wid(2)));
    s.switch(c);
    let pinned_before = s.reactor.contexts.pinned().to_vec();

    s.reactor
        .handle_event(Event::WindowTitleChanged(wid(2), "Renamed".to_string().into()));
    s.reactor.handle_event(Event::WindowTitleChanged(
        WindowId::new(1, 9),
        "Unknown".to_string().into(),
    ));

    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Renamed", RecordLink::Live(wid(2))),
        ],
        records(&s, c)
    );
    assert_eq!(vec![record("Renamed", RecordLink::Live(wid(2)))], records(&s, d));
    let pinned: Vec<(String, RecordLink)> =
        s.reactor.contexts.pinned().iter().map(|m| (m.title.clone(), m.link)).collect();
    assert_eq!(vec![record("Renamed", RecordLink::Live(wid(2)))], pinned);
    assert_eq!(1, pinned_before.len());
    assert_eq!("Renamed", s.desc(wid(2)).title);
    assert!(!s.reactor.windows.contains_key(&WindowId::new(1, 9)));
}

/// R23, R21, R28. While contexts are off, app 2 quits: its window, a member of
/// C, closes, and the app terminates. The app runs again, and contexts are
/// turned on while D is active. The window rejoins C by its title, as after a
/// quit while contexts are on, so it is parked under D, and switching to C
/// tiles it.
#[test]
fn r23_r28_a_member_whose_app_quits_while_contexts_are_off_rejoins_when_they_are_on() {
    let mut s = Setup::new(1);
    let doc = launch(&mut s, 2, test_app_info(2), vec![titled("Doc", 21, 700.)])[0];
    let c = s.create("C", &[wid(1), doc]);
    let d = s.create("D", &[wid(1)]);
    s.switch(c);
    s.switch(d);
    s.reactor.handle_event(Event::ConfigChanged(config(false)));
    s.apps.simulate_until_quiet(&mut s.reactor);
    s.close(doc);
    s.reactor.handle_event(Event::ApplicationTerminated(2));
    s.reactor.handle_event(Event::ApplicationThreadTerminated(2));
    s.apps.simulate_until_quiet(&mut s.reactor);
    let again = launch(&mut s, 5, test_app_info(2), vec![titled("Doc", 51, 700.)])[0];

    s.reactor.handle_event(Event::ConfigChanged(config(true)));
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert_eq!(vec![id_of(c)], s.reactor.contexts.contexts_of(again));
    assert_eq!(d, s.reactor.contexts.active());
    assert_eq!(vec![again], s.parked());
    s.switch(c);
    let tiles = halves(wid(1), again);
    assert_eq!(tiles, s.tiles());
    assert_eq!(tiles, s.frames(&[wid(1), again]));
}

/// L11, R28. With contexts off, a new window reaches the layout through
/// `WindowCreated`, the window list, and `WindowBecameVisible`, and gets one
/// tile.
#[test]
fn l11_with_contexts_off_a_window_that_arrives_three_ways_gets_one_tile() {
    let mut s = Setup::new(2);
    s.reactor.handle_event(Event::ConfigChanged(config(false)));
    s.apps.simulate_until_quiet(&mut s.reactor);
    let info = WindowInfo {
        frame: rect(700., 100., 50., 50.),
        ..make_window(3)
    };

    open_window(&mut s, wid(3), info, &[wid(1), wid(2)]);

    let thirds = vec![
        (wid(1), rect(0., 0., 400., 1000.)),
        (wid(2), rect(400., 0., 400., 1000.)),
        (wid(3), rect(800., 0., 400., 1000.)),
    ];
    assert_eq!(thirds, s.tiles());
    assert_eq!(thirds, s.frames(&[wid(1), wid(2), wid(3)]));
    assert!(s.reactor.contexts.is_unsorted(wid(3)));
}

/// The four membership commands, with `target` for the ones that take one.
fn membership_commands(target: ContextKey) -> [ContextCommand; 4] {
    [
        ContextCommand::AddWindowToContext(ContextRef::Id(id_of(target))),
        ContextCommand::MoveWindowToContext(ContextRef::Id(id_of(target))),
        ContextCommand::RemoveWindowFromContext,
        ContextCommand::ToggleWindowPinned,
    ]
}

/// R37, R14. The membership commands act only on a window that can be in a
/// context. For Sugarglider's own window, and for a panel on a layer of its
/// own, they change no record, park nothing, and write no `contexts.json`.
#[test]
fn r37_membership_commands_on_an_own_or_untracked_window_change_nothing() {
    let mut s = Setup::new(2);
    let own_pid = std::process::id() as i32;
    let own_window = vec![titled("Preferences", 50, 300.)];
    let own = launch(&mut s, own_pid, test_app_info(own_pid), own_window)[0];
    let panel = launch(&mut s, 3, test_app_info(3), vec![titled("Panel", 30, 500.)])[0];
    report_with_panel(&mut s, &[wid(1), wid(2), own, panel], panel);
    let c = s.create("C", &[wid(1), wid(2)]);
    let d = s.create("D", &[]);
    s.switch(c);
    let before = (records(&s, c), records(&s, d));
    let path = s.dir.path().join("contexts.json");
    fs::remove_file(&path).unwrap();

    for window in [own, panel] {
        focus_quietly(&mut s, window);
        for command in membership_commands(d) {
            run(&mut s, command);
        }
        s.apps.simulate_until_quiet(&mut s.reactor);
    }

    assert_eq!(before, (records(&s, c), records(&s, d)));
    assert!(s.reactor.contexts.pinned().is_empty());
    assert!(s.reactor.contexts.is_unsorted(own));
    assert!(s.reactor.contexts.is_unsorted(panel));
    assert!(s.parked().is_empty());
    assert!(!path.exists());
    // R29: neither window keeps Unsorted listed.
    assert!(!s.reactor.lists_unsorted());
}

/// R37. A membership command acts on the focused window only, and adding a
/// window twice keeps one record. Once the app is deactivated no window has
/// the focus, and the commands do nothing.
#[test]
fn r37_a_membership_command_acts_on_the_focused_window_only() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1), wid(2), wid(3)]);
    let d = s.create("D", &[]);
    s.switch(c);
    focus_quietly(&mut s, wid(2));

    for _ in 0..2 {
        run(
            &mut s,
            ContextCommand::AddWindowToContext(ContextRef::Id(id_of(d))),
        );
    }
    assert_eq!(vec![record("Window2", RecordLink::Live(wid(2)))], records(&s, d));

    s.reactor.handle_event(Event::ApplicationGloballyDeactivated(1));
    assert_eq!(None, s.reactor.main_window());
    for command in membership_commands(d) {
        run(&mut s, command);
    }
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert_eq!(vec![record("Window2", RecordLink::Live(wid(2)))], records(&s, d));
    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Window2", RecordLink::Live(wid(2))),
            record("Window3", RecordLink::Live(wid(3))),
        ],
        records(&s, c)
    );
    assert!(s.reactor.contexts.pinned().is_empty());
    assert!(s.parked().is_empty());
}

/// R37, R4, commands and dispatch. A membership command names its target as a
/// switch does: by number, by id, or by name, ignoring case and accents, with
/// a partial name taking the context it ranks first. Everything and Unsorted
/// in any case, a blank name, a name that matches nothing, a free number, and
/// the id of a deleted context are no target, and the command does nothing.
#[test]
fn r37_a_membership_command_names_its_target_like_a_switch() {
    let mut s = Setup::new(3);
    let comms = s.create("Comms", &[wid(1)]);
    let client = s.create("Café Client", &[wid(1)]);
    let gone = s.create("Gone", &[]);
    s.reactor.contexts.delete(id_of(gone)).unwrap();
    focus_quietly(&mut s, wid(2));
    let add = |reference: ContextRef| ContextCommand::AddWindowToContext(reference);

    for name in [
        "everything",
        " EVERYTHING ",
        "Unsorted",
        "unsorted",
        "",
        "  ",
        "zzz",
    ] {
        run(&mut s, add(ContextRef::Name(name.into())));
    }
    run(&mut s, add(ContextRef::Number(9)));
    run(&mut s, add(ContextRef::Id(id_of(gone))));
    assert_eq!(
        vec![record("Window1", RecordLink::Live(wid(1)))],
        records(&s, comms)
    );
    assert_eq!(
        vec![record("Window1", RecordLink::Live(wid(1)))],
        records(&s, client)
    );

    run(&mut s, add(ContextRef::Name("COMMS".into())));
    run(&mut s, add(ContextRef::Name("cafe cl".into())));
    focus_quietly(&mut s, wid(3));
    run(&mut s, add(ContextRef::Number(2)));

    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Window2", RecordLink::Live(wid(2))),
        ],
        records(&s, comms)
    );
    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Window2", RecordLink::Live(wid(2))),
            record("Window3", RecordLink::Live(wid(3))),
        ],
        records(&s, client)
    );
    assert!(s.parked().is_empty());
}

/// R37. Moving a window to the context that is active changes nothing. Under
/// Everything no context is active to leave, so moving only adds, and nothing
/// is parked. Under Unsorted, moving a window into D takes it out of
/// Unsorted, so it is parked.
#[test]
fn r37_moving_a_window_to_the_active_context_or_under_a_built_in_entry() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1), wid(2)]);
    let d = s.create("D", &[]);
    s.switch(c);
    let in_c = halves(wid(1), wid(2));
    focus_quietly(&mut s, wid(2));
    run(
        &mut s,
        ContextCommand::MoveWindowToContext(ContextRef::Id(id_of(c))),
    );
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Window2", RecordLink::Live(wid(2))),
        ],
        records(&s, c)
    );
    assert_eq!(Vec::<(String, RecordLink)>::new(), records(&s, d));
    assert_eq!(vec![wid(3)], s.parked());
    assert_eq!(in_c, s.tiles());
    assert_eq!(in_c, s.frames(&[wid(1), wid(2)]));

    s.switch(ContextKey::Everything);
    focus_quietly(&mut s, wid(2));
    run(
        &mut s,
        ContextCommand::MoveWindowToContext(ContextRef::Id(id_of(d))),
    );
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(vec![id_of(c), id_of(d)], s.reactor.contexts.contexts_of(wid(2)));
    assert!(s.parked().is_empty());

    s.switch(ContextKey::Unsorted);
    assert_eq!(vec![wid(1), wid(2)], s.parked());
    focus_quietly(&mut s, wid(3));
    run(
        &mut s,
        ContextCommand::MoveWindowToContext(ContextRef::Id(id_of(d))),
    );
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(vec![id_of(d)], s.reactor.contexts.contexts_of(wid(3)));
    assert_eq!(vec![wid(1), wid(2), wid(3)], s.parked());
    assert_eq!(Vec::<(WindowId, CGRect)>::new(), s.tiles());
}

/// R37, R3. Removing the focused window from C leaves it showing when it is
/// pinned, and parks it when it is only in D.
#[test]
fn r37_removing_a_pinned_window_keeps_it_and_a_window_of_another_context_is_parked() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1), wid(2), wid(3)]);
    let d = s.create("D", &[wid(3)]);
    s.reactor.contexts.pin(&s.desc(wid(2)));
    s.switch(c);
    let thirds = s.tiles();
    focus_quietly(&mut s, wid(2));

    run(&mut s, ContextCommand::RemoveWindowFromContext);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Window3", RecordLink::Live(wid(3))),
        ],
        records(&s, c)
    );
    assert!(s.reactor.contexts.is_pinned(wid(2)));
    assert!(s.parked().is_empty());
    assert_eq!(thirds, s.tiles());

    focus_quietly(&mut s, wid(3));
    run(&mut s, ContextCommand::RemoveWindowFromContext);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(vec![record("Window1", RecordLink::Live(wid(1)))], records(&s, c));
    assert_eq!(vec![id_of(d)], s.reactor.contexts.contexts_of(wid(3)));
    assert_eq!(vec![wid(3)], s.parked());
    let tiles = halves(wid(1), wid(2));
    assert_eq!(tiles, s.tiles());
    assert_eq!(tiles, s.frames(&[wid(1), wid(2)]));
}

/// R3. A window pinned with `toggle_window_pinned` is a member of a context
/// created after it was pinned, and shows there next to that context's
/// member, which is placed on the side where it was before it was parked.
#[test]
fn r3_a_window_pinned_by_command_shows_in_a_context_created_later() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1), wid(3)]);
    s.switch(c);
    focus_quietly(&mut s, wid(3));
    run(&mut s, ContextCommand::ToggleWindowPinned);
    assert!(s.reactor.contexts.is_pinned(wid(3)));

    let e = s.create("E", &[wid(2)]);
    s.switch(e);

    assert_eq!(vec![wid(1)], s.parked());
    let tiles = halves(wid(2), wid(3));
    assert_eq!(tiles, s.tiles());
    assert_eq!(tiles, s.frames(&[wid(2), wid(3)]));
}

/// R3, R29. A pinned window shows under Unsorted but doesn't count as
/// unsorted: while the only window in no named context is pinned, Unsorted
/// isn't listed, and a switch to it by name does nothing.
#[test]
fn r3_r29_a_pinned_window_doesnt_keep_unsorted_listed() {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1)]);
    assert!(s.reactor.lists_unsorted());
    focus_quietly(&mut s, wid(2));
    run(&mut s, ContextCommand::ToggleWindowPinned);
    s.switch(c);
    assert!(s.parked().is_empty());

    assert!(!s.reactor.lists_unsorted());
    switch_by_name(&mut s, "Unsorted");
    assert!(s.apps.requests().is_empty());
    assert_eq!(c, s.reactor.contexts.active());
}

/// R3, R37. Unpinning under Everything parks nothing. Unpinning under
/// Unsorted parks a window that is in a named context, because it no longer
/// shows there, and leaves the unsorted window.
#[test]
fn r3_unpinning_under_unsorted_parks_a_window_of_a_named_context() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1), wid(2)]);
    focus_quietly(&mut s, wid(2));
    run(&mut s, ContextCommand::ToggleWindowPinned);
    run(&mut s, ContextCommand::ToggleWindowPinned);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert!(!s.reactor.contexts.is_pinned(wid(2)));
    assert!(s.parked().is_empty());
    run(&mut s, ContextCommand::ToggleWindowPinned);

    s.switch(ContextKey::Unsorted);
    assert_eq!(vec![wid(1)], s.parked());
    focus_quietly(&mut s, wid(2));
    run(&mut s, ContextCommand::ToggleWindowPinned);
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert_eq!(vec![id_of(c)], s.reactor.contexts.contexts_of(wid(2)));
    assert!(!s.reactor.contexts.is_pinned(wid(2)));
    assert_eq!(vec![wid(1), wid(2)], s.parked());
    assert_eq!(vec![(wid(3), screen())], s.tiles());
    assert_eq!(screen(), s.frame(wid(3)));
}

/// R37. Under Unsorted, the focused window 2 is added to C. The user then
/// focuses window 3, so window 2 is no longer the main window. Until the next
/// switch, window 2 still counts as a member of Unsorted: a window list and
/// the refresh that asks the apps for their windows leave it where it is,
/// with its tile.
#[test]
fn r37_an_added_window_keeps_showing_after_it_loses_the_focus() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1)]);
    s.switch(ContextKey::Unsorted);
    let unsorted = halves(wid(2), wid(3));
    assert_eq!(unsorted, s.tiles());
    focus_quietly(&mut s, wid(2));
    run(
        &mut s,
        ContextCommand::AddWindowToContext(ContextRef::Id(id_of(c))),
    );
    s.apps.simulate_until_quiet(&mut s.reactor);
    focus_quietly(&mut s, wid(3));

    report_visible(&mut s, &[wid(1), wid(2), wid(3)]);
    let on_screen = on_screen(&s, &[wid(1), wid(2), wid(3)]);
    s.reactor
        .handle_event(Event::WindowsOnScreenUpdated { pid: Some(1), on_screen });
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert_eq!(vec![id_of(c)], s.reactor.contexts.contexts_of(wid(2)));
    assert_eq!(vec![wid(1)], s.parked());
    assert_eq!(unsorted, s.tiles());
    assert_eq!(unsorted, s.frames(&[wid(2), wid(3)]));
}

/// R22, R28. A title change while contexts are off doesn't reach the member
/// records or the window rules: tracking runs only with the flag on. After
/// contexts are turned on and the app quits, the record keeps the title from
/// before, so a relaunch under the new title rejoins nothing and is parked
/// under the active context.
#[test]
fn r22_r28_a_title_change_while_contexts_are_off_is_never_tracked() {
    let mut s = Setup::new(1);
    let doc = launch(&mut s, 2, test_app_info(2), vec![titled("Draft", 21, 700.)])[0];
    let c = s.create("C", &[wid(1), doc]);
    let d = s.create("D", &[wid(1)]);
    s.switch(c);
    s.reactor.handle_event(Event::ConfigChanged(config(false)));
    s.apps.simulate_until_quiet(&mut s.reactor);
    s.reactor
        .handle_event(Event::WindowTitleChanged(doc, "Final report".to_string().into()));
    s.reactor.handle_event(Event::ConfigChanged(config(true)));
    s.apps.simulate_until_quiet(&mut s.reactor);
    s.switch(d);
    s.close(doc);
    s.reactor.handle_event(Event::ApplicationTerminated(2));
    s.reactor.handle_event(Event::ApplicationThreadTerminated(2));
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(vec!["Window1", "Draft"], saved_members(&s, c));

    let again = launch(
        &mut s,
        5,
        test_app_info(2),
        vec![titled("Final report", 51, 700.)],
    )[0];

    // The window matches no record, so it joins D, the active context, and
    // doesn't rejoin C.
    assert_eq!(vec![id_of(d)], s.reactor.contexts.contexts_of(again));
    assert!(s.parked().is_empty());
    assert_eq!(vec!["Window1", "Draft"], saved_members(&s, c));
}

/// R28. The reactor tells the app to send title changes only while contexts
/// are on, so with contexts off the window rules never see one.
#[test]
fn r28_the_app_tracks_titles_only_while_contexts_are_on() {
    let mut s = Setup::on(vec![screen()], vec![Some(space())]);
    s.reactor.handle_events(s.apps.make_app(1, make_windows(1)));
    let launched = s.apps.requests();
    assert!(
        launched.iter().any(|request| matches!(request, Request::TrackTitles(true))),
        "{launched:?}"
    );

    s.reactor.handle_event(Event::ConfigChanged(config(false)));
    let off = s.apps.requests();
    assert!(
        off.iter().any(|request| matches!(request, Request::TrackTitles(false))),
        "{off:?}"
    );

    s.reactor.handle_event(Event::ConfigChanged(config(true)));
    let on = s.apps.requests();
    assert!(
        on.iter().any(|request| matches!(request, Request::TrackTitles(true))),
        "{on:?}"
    );
}

/// R28. With contexts off, a title change doesn't reach the window rules: a
/// window that a rule floats by a new title is renamed, minimized, and
/// unminimized, and comes back tiled, because the reactor kept the title
/// from creation.
#[test]
fn r28_with_contexts_off_a_title_change_doesnt_change_a_rules_classification() {
    use crate::config::{WindowRule, WindowRuleConditions};
    let mut config = Config::default();
    config.settings.default_disable = false;
    config.settings.animate = false;
    config.window_rules = vec![WindowRule {
        conditions: WindowRuleConditions {
            title_substring: Some("Preferences".into()),
            ..Default::default()
        },
        float: true,
    }];
    let mut s = Setup::on(vec![screen()], vec![Some(space())]);
    s.reactor.handle_event(Event::ConfigChanged(Arc::new(config)));
    s.reactor.handle_events(s.apps.make_app(1, make_windows(2)));
    s.reactor.handle_event(Event::StartupComplete);
    s.apps.simulate_until_quiet(&mut s.reactor);
    let tiled = |reactor: &Reactor| -> Vec<WindowId> {
        let mut tiles: Vec<WindowId> = reactor
            .layout
            .calculate_layout(space(), screen(), &reactor.config)
            .into_iter()
            .map(|(wid, _)| wid)
            .collect();
        tiles.sort();
        tiles
    };
    assert_eq!(vec![wid(1), wid(2)], tiled(&s.reactor));

    s.reactor.handle_event(Event::WindowTitleChanged(
        wid(2),
        "Preferences".to_string().into(),
    ));
    let listed = on_screen(&s, &[wid(1)]);
    s.reactor.handle_event(Event::WindowsOnScreenUpdated {
        pid: Some(1),
        on_screen: listed,
    });
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(vec![wid(1)], tiled(&s.reactor));

    report_visible(&mut s, &[wid(1), wid(2)]);

    assert_eq!(vec![wid(1), wid(2)], tiled(&s.reactor));
}

/// R36, H5. A window of the app shows where the app's parked window sits,
/// which a tiling layout can produce. Parked windows share a corner without
/// being tabs, so the showing window takes no membership from the parked
/// one, and a membership command on it leaves the parked window alone.
#[test]
fn r36_parked_windows_that_share_a_corner_are_no_tabs() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1), wid(2)]);
    let d = s.create("D", &[wid(3)]);
    s.switch(c);
    assert_eq!(vec![wid(3)], s.parked());
    let corner = s.frame(wid(3));
    s.reactor.windows.get_mut(&wid(2)).unwrap().frame_monotonic = corner;
    s.apps.windows.get_mut(&wid(2)).unwrap().frame = corner;

    assert_eq!(vec![wid(2)], s.reactor.tabs_of(wid(2)));
    assert_eq!(wid(2), s.reactor.membership_window(wid(2)));
    let e = s.create("E", &[]);
    focus_quietly(&mut s, wid(2));
    run(
        &mut s,
        ContextCommand::AddWindowToContext(ContextRef::Id(id_of(e))),
    );

    assert_eq!(vec![id_of(c), id_of(e)], s.reactor.contexts.contexts_of(wid(2)));
    assert_eq!(vec![id_of(d)], s.reactor.contexts.contexts_of(wid(3)));
    assert_eq!(vec![wid(3)], s.reactor.tabs_of(wid(3)));
}

/// R20, H5. D holds window 2, which is parked while C is active. App 1 opens
/// window 3 at window 2's corner, as an app that restores the frame its last
/// window had can do. Parked windows share a corner without being tabs, so
/// window 3 matches no record, joins C, the active context, and gets a tile.
#[test]
fn r20_h5_a_new_window_at_a_parked_windows_corner_joins_the_active_context() {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1)]);
    s.create("D", &[wid(2)]);
    s.switch(c);
    let parked_at = corner(CGSize::new(600., 1000.));
    assert_eq!(parked_at, s.frame(wid(2)));
    let info = WindowInfo {
        frame: parked_at,
        ..make_window(3)
    };

    open_window(&mut s, wid(3), info, &[wid(1), wid(2)]);

    assert_eq!(vec![id_of(c)], s.reactor.contexts.contexts_of(wid(3)));
    assert_eq!(vec![wid(2)], s.parked());
    let tiles = halves(wid(1), wid(3));
    assert_eq!(tiles, s.tiles());
    assert_eq!(tiles, s.frames(&[wid(1), wid(3)]));
}

/// R20, R36. D holds window 2, which the user minimized while it had the
/// right half of the screen. Under C, app 1 opens window 3 at that frame, as
/// an app that restores its last window frame can do. A minimized window
/// shows no frame, so it is no tab of the new window: window 3 matches no
/// record, joins C, and shows. It is never parked (R20).
#[test]
fn r20_r36_a_new_window_at_a_minimized_windows_frame_joins_the_active_context() {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1)]);
    s.create("D", &[wid(2)]);
    report_visible(&mut s, &[wid(1)]);
    s.switch(c);
    assert!(s.parked().is_empty());
    let info = WindowInfo {
        frame: s.frame(wid(2)),
        ..make_window(3)
    };

    open_window(&mut s, wid(3), info, &[wid(1)]);

    assert_eq!(vec![id_of(c)], s.reactor.contexts.contexts_of(wid(3)));
    assert!(s.parked().is_empty());
    let tiles = halves(wid(1), wid(3));
    assert_eq!(tiles, s.tiles());
    assert_eq!(tiles, s.frames(&[wid(1), wid(3)]));
}

/// R20, R36, R24. Window 2 of D is minimized at the frame it had on the
/// right half of the screen. Under C, app 1 opens window 3 at that frame and
/// makes it the main window, as an app that restores its last window frame
/// does. The minimized window is no tab: window 3 matches no record, joins C
/// and takes the focus there, and window 2 keeps its membership in D.
#[test]
fn r20_r36_a_new_focused_window_at_a_minimized_windows_frame_joins_the_active_context() {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[wid(2)]);
    let minimized_at = s.frame(wid(2));
    report_visible(&mut s, &[wid(1)]);
    s.switch(c);
    assert!(s.parked().is_empty());
    s.reactor.handle_event(Event::RaiseFocusSent {
        sequence_id: s.reactor.raise_sequence,
    });
    s.reactor.handle_event(Event::RaiseTimeout {
        sequence_id: s.reactor.raise_sequence,
    });
    s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
    s.reactor.handle_event(Event::ApplicationActivated(1, Quiet::No));
    s.reactor
        .handle_event(Event::ApplicationMainWindowChanged(1, Some(wid(1)), Quiet::No));
    assert_eq!(c, s.reactor.contexts.active());

    s.reactor
        .handle_event(Event::ApplicationMainWindowChanged(1, Some(wid(3)), Quiet::No));
    let info = WindowInfo {
        frame: minimized_at,
        ..make_window(3)
    };
    open_window(&mut s, wid(3), info, &[wid(1)]);

    assert_eq!(vec![id_of(c)], s.reactor.contexts.contexts_of(wid(3)));
    assert_eq!(c, s.reactor.contexts.active());
    assert_eq!(vec![id_of(d)], s.reactor.contexts.contexts_of(wid(2)));
    assert!(s.parked().is_empty());
    let tiles = halves(wid(1), wid(3));
    assert_eq!(tiles, s.tiles());
    assert_eq!(tiles, s.frames(&[wid(1), wid(3)]));
}

/// R36, R13. App 1 has window 1 on another Space, only in D, and window 2 on
/// the visible Space, only in C. Each fills the screen on its own Space, so
/// they have the same frame. Window 1 is still the app's main window. A
/// window on a Space nobody sees is no tab, so a switch to C shows window 2,
/// its member, and parks nothing.
///
/// Then the user changes to Space 1, where window 1 becomes visible. It is
/// only in D, so under C it must not show: it is parked and gets no tile.
#[test]
fn r36_r13_a_window_on_another_space_at_the_same_frame_is_no_tab() {
    let mut s = Setup::on(vec![screen()], vec![Some(space())]);
    let full = |idx: usize| WindowInfo {
        frame: screen(),
        ..make_window(idx)
    };
    let mut launch = s.apps.make_app_with_opts(1, vec![full(1), full(2)], Some(wid(1)), false);
    for event in &mut launch {
        if let Event::WindowsOnScreenUpdated { on_screen, .. } = event {
            on_screen.info.remove(0);
            on_screen.visible.remove(0);
        }
    }
    s.reactor.handle_events(launch);
    s.reactor.handle_event(Event::StartupComplete);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(vec![(wid(2), screen())], s.tiles());
    let c = s.create("C", &[wid(2)]);
    s.create("D", &[wid(1)]);

    s.switch(c);

    assert_eq!(Vec::<WindowId>::new(), s.parked());
    assert_eq!(vec![(wid(2), screen())], s.tiles());
    assert_eq!(screen(), s.frame(wid(2)));

    let snapshot = on_screen(&s, &[wid(1)]);
    s.reactor.handle_event(Event::SpaceChanged(vec![Some(space())], snapshot));
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert!(!s.reactor.contexts.is_member(c, wid(1)));
    assert_eq!(vec![wid(1)], s.parked());
    assert!(s.tiles().is_empty());
    assert_eq!(corner(screen().size), s.frame(wid(1)));
}

/// R22 step 4, R29. C holds an empty record of app 2 that no title matches. App
/// 2's window 1 was closed with ⌘W and never reported destroyed, so its app no
/// longer lists it, and it counts as gone. A switch to C fills the record with
/// window 2, the app's open window that is in no context, and window 2 shows.
#[test]
fn r22_step_4_fills_a_record_with_an_open_window_and_not_a_closed_one() {
    let mut s = Setup::new(1);
    let c = s.create("C", &[wid(1)]);
    empty_record(&mut s, c, WindowId::new(90, 1), 2, "Compose");
    let windows = vec![titled("Draft", 21, 700.), titled("Inbox", 22, 900.)];
    let [closed, open] = launch(&mut s, 2, test_app_info(2), windows)[..] else {
        panic!()
    };
    s.apps.windows.remove(&closed);
    s.reactor.handle_event(Event::WindowsDiscovered {
        pid: 2,
        new: vec![],
        known_visible: vec![open],
    });
    report_visible(&mut s, &[wid(1), open]);

    s.switch(c);

    assert_eq!(
        vec![
            record("Window1", RecordLink::Live(wid(1))),
            record("Inbox", RecordLink::Live(open)),
        ],
        records(&s, c)
    );
    assert_eq!(Vec::<WindowId>::new(), s.parked());
    let tiles = halves(wid(1), open);
    assert_eq!(tiles, s.tiles());
    assert_eq!(tiles, s.frames(&[wid(1), open]));
}

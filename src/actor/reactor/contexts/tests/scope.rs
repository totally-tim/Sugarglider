// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reactor tests for `per_screen` scope: each screen has its own active
//! context (R8), a switch takes the target's members along from the other
//! screens and keeps their place in the layouts they leave (R9), focus from
//! outside switches the screen a window was last shown on (R26), and a scope
//! change follows R11.

use test_log::test;

use super::*;

/// Two displays side by side, each with its own Space, in `per_screen`
/// scope. Apps 1 and 2 each have a window on the left display and one on the
/// right.
fn per_screen_setup() -> Setup {
    let mut s = two_displays_two_apps();
    s.scope(Scope::PerScreen);
    s
}

fn right_space() -> SpaceId {
    SpaceId::new(2)
}

/// Whether the frame is on the right display.
fn on_right(frame: CGRect) -> bool {
    frame.origin.x >= 1200.0
}

/// R8, R9. A `per_screen` switch changes only the screen it runs on: the
/// target's members come to it from the other screen, its own non-members
/// are parked there, and the other screen keeps what it shows.
#[test]
fn r8_r9_a_per_screen_switch_changes_only_the_screen_it_runs_on() {
    let mut s = per_screen_setup();
    let left_window = wid(1);
    let right_window = WindowId::new(2, 2);
    let c = s.create("C", &[left_window, right_window]);
    let id = id_of(c);
    s.focus_screen(1);

    s.switch(c);

    // The right screen shows C with its member and the member that came
    // over. The member's own frame lands on the right display.
    assert_eq!(ContextKey::Everything, s.active_on(1));
    assert_eq!(ContextKey::Named(id), s.active_on(2));
    assert!(on_right(s.frame(left_window)), "{:?}", s.frame(left_window));
    let right_tiles = s.tiles_on(right_space(), right());
    assert_eq!(2, right_tiles.len());
    assert!(right_tiles.iter().all(|(_, frame)| on_right(*frame)));
    // The right screen's non-member is parked, with its journal entry.
    assert_eq!(vec![wid(2)], s.parked());
    assert_eq!(vec![entry(2, rect(1200., 0., 600., 1000.))], s.journal_on_disk());
    // The left screen still shows Everything, so its window stays: it is not
    // in C.
    assert_eq!(
        vec![(WindowId::new(2, 1), screen())],
        s.tiles_on(space(), screen())
    );
    assert!(!on_right(s.frame(WindowId::new(2, 1))));
    // The active contexts are saved per screen.
    assert_eq!(ContextKey::Named(id), s.saved_active_on(2));
}

/// R9: a member that moves to another display keeps its
/// place in the layout it left, which closes the gap while it is away. When
/// its screen switches back, it returns to that place.
#[test]
fn r9_a_moved_member_returns_to_its_place_in_the_layout_it_left() {
    let mut s = Setup::on(vec![screen(), right()], vec![Some(space()), Some(right_space())]);
    let middle = wid(2);
    let windows = [
        (1, rect(1300., 100., 50., 50.)),
        (2, rect(1500., 100., 50., 50.)),
        (3, rect(1700., 100., 50., 50.)),
    ];
    let windows = windows
        .into_iter()
        .map(|(idx, frame)| WindowInfo {
            sys_id: Some(WindowServerId::new(idx as u32 * 10 + 1)),
            frame,
            ..make_window(idx)
        })
        .collect();
    s.reactor.handle_events(s.apps.make_app(1, windows));
    s.reactor.handle_event(Event::StartupComplete);
    s.apps.simulate_until_quiet(&mut s.reactor);
    s.scope(Scope::PerScreen);
    let all = [wid(1), middle, wid(3)];
    let c = s.create("C", &all);
    s.focus_screen(1);
    s.switch(c);
    let right_before = s.frames(&all);
    assert_eq!(3, s.tiles_on(right_space(), right()).len());

    // The middle window belongs to a context the left screen switches to, so
    // it leaves the right screen. Its layout closes the gap.
    let d = s.create("D", &[middle]);
    s.focus_screen(0);
    s.switch(d);

    assert!(s.frame(middle).origin.x < 1200.0, "{:?}", s.frame(middle));
    assert!(!s.reactor.layout.has_node_in(right_space(), c, middle));
    assert_eq!(2, s.tiles_on(right_space(), right()).len());

    // The right screen switches to C again: the window comes back to the
    // place it held between the other two.
    s.focus_screen(1);
    s.switch(c);

    assert_eq!(right_before, s.frames(&all));
    assert_eq!(3, s.tiles_on(right_space(), right()).len());
}

/// R16. Switching to the context a screen already shows applies it again
/// there, and leaves the other screens alone.
#[test]
fn r16_a_second_switch_on_one_screen_leaves_the_other_screen_alone() {
    let mut s = per_screen_setup();
    let c = s.create("C", &[wid(1), WindowId::new(2, 2)]);
    let id = id_of(c);
    s.focus_screen(1);
    s.switch(c);
    let left_window = WindowId::new(2, 1);

    s.switch(c);

    assert_eq!(ContextKey::Named(id), s.active_on(2));
    assert_eq!(ContextKey::Everything, s.active_on(1));
    assert_eq!(vec![(left_window, screen())], s.tiles_on(space(), screen()));
}

/// R11. Going to `per_screen` gives every screen the global context. Going
/// back makes the focused screen's context the global one.
#[test]
fn r11_changing_scope_spreads_and_keeps_the_shown_contexts() {
    let mut s = two_displays_two_apps();
    let c = s.create("C", &[wid(1), WindowId::new(2, 2)]);
    let d = s.create("D", &[wid(2), WindowId::new(2, 1)]);
    s.switch(c);
    assert_eq!(ContextKey::Named(id_of(c)), s.reactor.shown_context(space()));
    assert_eq!(
        ContextKey::Named(id_of(c)),
        s.reactor.shown_context(right_space())
    );

    s.scope(Scope::PerScreen);

    assert_eq!(ContextKey::Named(id_of(c)), s.active_on(1));
    assert_eq!(ContextKey::Named(id_of(c)), s.active_on(2));
    s.focus_screen(1);
    s.switch(d);
    assert_eq!(ContextKey::Named(id_of(d)), s.active_on(2));
    assert_eq!(ContextKey::Named(id_of(c)), s.active_on(1));

    s.scope(Scope::Global);

    assert_eq!(ContextKey::Named(id_of(d)), s.reactor.shown_context(space()));
    assert_eq!(
        ContextKey::Named(id_of(d)),
        s.reactor.shown_context(right_space())
    );
}

/// On a cold change to global scope, the focused screen at startup supplies
/// the global context. The saved per-screen map must be cleared before the
/// next switch is written, or a second launch loses that switch.
#[test]
fn a_cold_change_to_global_uses_the_focused_screen_and_persists_switches() {
    for (focused, expected, stale_focus) in [(0, "A", false), (1, "B", false), (1, "A", true)] {
        let mut s = per_screen_setup();
        let a = s.create("A", &[wid(1)]);
        let b = s.create("B", &[WindowId::new(2, 2)]);
        let c = s.create("C", &[wid(2)]);
        s.focus_screen(0);
        s.switch(a);
        s.focus_screen(1);
        s.switch(b);
        assert_eq!(b, s.saved_active_on(2));

        let mut config = Config::default();
        config.settings.default_disable = false;
        config.settings.animate = false;
        config.settings.experimental.contexts.enable = true;
        let Setup { dir, .. } = s;
        let mut restarted = Setup {
            reactor: Reactor::new_for_test(LayoutManager::new_for_test()),
            apps: Apps::new(),
            dir,
        };
        restarted.reactor.journal =
            ParkedJournal::open(restarted.dir.path().join("parked.json"), SystemTime::now());
        restarted.reactor.handle_event(Event::ConfigChanged(Arc::new(config)));
        let store = ContextsStore::new(restarted.dir.path().join("contexts.json"));
        restarted.reactor.open_contexts(store, Some("boot".into()), SystemTime::now());
        restarted.reactor.handle_event(screens(
            vec![screen(), right()],
            vec![Some(space()), Some(right_space())],
        ));
        if focused == 1 {
            let error = restarted
                .reactor
                .run_context_command(ContextCommand::ToggleWindowPinned)
                .unwrap_err();
            assert_eq!("Contexts are waiting for the focused screen at startup", error);
            assert!(restarted.reactor.contexts.has_screen_actives());
        }
        let pid = 3;
        let focused_window = WindowId::new(pid, 1);
        let x = if focused == 0 { 100. } else { 1400. };
        let window = WindowInfo {
            sys_id: Some(WindowServerId::new(31)),
            frame: rect(x, 100., 50., 50.),
            ..make_window(1)
        };
        restarted.reactor.handle_events(restarted.apps.make_app(pid, vec![window]));
        restarted.apps.simulate_until_quiet(&mut restarted.reactor);
        restarted.reactor.handle_event(Event::ApplicationGloballyActivated(pid));
        restarted.reactor.handle_event(Event::ApplicationActivated(pid, Quiet::Yes));
        restarted.reactor.handle_event(Event::ApplicationMainWindowChanged(
            pid,
            Some(focused_window),
            Quiet::Yes,
        ));
        assert_eq!(Some(focused_window), restarted.reactor.main_window());
        if stale_focus {
            restarted.reactor.update_active_screen();
            assert_eq!(Some(1), restarted.reactor.active_screen_idx);
            restarted.reactor.handle_event(Event::ApplicationMainWindowChanged(
                pid,
                None,
                Quiet::Yes,
            ));
            assert_eq!(None, restarted.reactor.main_window());
            let error = restarted
                .reactor
                .run_context_command(ContextCommand::ToggleWindowPinned)
                .unwrap_err();
            assert_eq!("Contexts are waiting for the focused screen at startup", error);
            assert!(restarted.reactor.contexts.has_screen_actives());
        }
        if focused == 0 {
            restarted.reactor.update_active_screen();
            assert_eq!(Some(0), restarted.reactor.active_screen_idx);
            restarted.reactor.handle_event(Event::ApplicationMainWindowChanged(
                pid,
                None,
                Quiet::Yes,
            ));
            assert_eq!(None, restarted.reactor.main_window());
            let error = restarted
                .reactor
                .run_context_command(ContextCommand::ToggleWindowPinned)
                .unwrap_err();
            assert_eq!("Contexts are waiting for the focused screen at startup", error);
            assert!(restarted.reactor.contexts.has_screen_actives());
            restarted.reactor.handle_event(Event::ApplicationMainWindowChanged(
                pid,
                Some(focused_window),
                Quiet::Yes,
            ));
        }
        if focused == 1 && !stale_focus {
            restarted.reactor.handle_event(Event::Command(Command::Context(
                ContextCommand::RenameContext {
                    context: ContextRef::Id(id_of(a)),
                    name: "Renamed A".into(),
                },
            )));
            assert!(restarted.reactor.contexts.by_name("Renamed A").is_some());
            assert_eq!(b, restarted.reactor.contexts.active());
            assert!(!restarted.reactor.contexts.has_screen_actives());
        }
        restarted.reactor.handle_event(Event::StartupComplete);
        restarted.apps.simulate_until_quiet(&mut restarted.reactor);

        let selected = restarted.reactor.contexts.by_name(expected).unwrap().id;
        assert_eq!(ContextKey::Named(selected), restarted.reactor.contexts.active());
        assert_eq!(ContextKey::Named(selected), restarted.saved_active());
        assert_eq!(3, restarted.reactor.contexts.contexts().len());

        restarted.switch(c);
        assert_eq!(c, restarted.saved_active());
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(restarted.dir.path().join("contexts.json")).unwrap())
                .unwrap();
        assert!(saved["active"].get("per_screen").is_none());

        let store = ContextsStore::new(restarted.dir.path().join("contexts.json"));
        restarted.reactor.open_contexts(store, Some("boot".into()), SystemTime::now());
        assert_eq!(c, restarted.reactor.contexts.active());
    }
}

/// If startup completes before screens appear, the first screen event must
/// apply the chosen context after it reconciles the saved per-screen map.
#[test]
fn a_late_screen_event_applies_the_focused_cold_global_context() {
    let mut s = per_screen_setup();
    let a = s.create("A", &[wid(1)]);
    let b = s.create("B", &[WindowId::new(2, 2)]);
    s.focus_screen(0);
    s.switch(a);
    s.focus_screen(1);
    s.switch(b);

    let mut config = Config::default();
    config.settings.default_disable = false;
    config.settings.animate = false;
    config.settings.experimental.contexts.enable = true;
    let Setup { dir, .. } = s;
    let mut restarted = Setup {
        reactor: Reactor::new_for_test(LayoutManager::new_for_test()),
        apps: Apps::new(),
        dir,
    };
    restarted.reactor.journal =
        ParkedJournal::open(restarted.dir.path().join("parked.json"), SystemTime::now());
    restarted.reactor.handle_event(Event::ConfigChanged(Arc::new(config)));
    let store = ContextsStore::new(restarted.dir.path().join("contexts.json"));
    restarted.reactor.open_contexts(store, Some("boot".into()), SystemTime::now());
    restarted.reactor.handle_event(Event::StartupComplete);
    assert!(restarted.reactor.contexts.has_screen_actives());

    let pid = 3;
    let focused_window = WindowId::new(pid, 1);
    let window = WindowInfo {
        sys_id: Some(WindowServerId::new(31)),
        frame: rect(1400., 100., 50., 50.),
        ..make_window(1)
    };
    restarted.reactor.handle_events(restarted.apps.make_app(pid, vec![window]));
    restarted.apps.simulate_until_quiet(&mut restarted.reactor);
    restarted.reactor.handle_event(Event::ApplicationGloballyActivated(pid));
    restarted.reactor.handle_event(Event::ApplicationActivated(pid, Quiet::Yes));
    restarted.reactor.handle_event(Event::ApplicationMainWindowChanged(
        pid,
        Some(focused_window),
        Quiet::Yes,
    ));
    assert_eq!(Some(focused_window), restarted.reactor.main_window());

    restarted.reactor.handle_event(screens(
        vec![screen(), right()],
        vec![Some(space()), Some(right_space())],
    ));
    restarted.apps.simulate_until_quiet(&mut restarted.reactor);

    assert_eq!(b, restarted.reactor.contexts.active());
    assert_eq!(b, restarted.reactor.shown_context(space()));
    assert_eq!(b, restarted.reactor.shown_context(right_space()));
    assert_eq!(b, restarted.saved_active());
    assert_eq!(b, restarted.reactor.layout.active_context_for_test(space()));
    assert_eq!(
        b,
        restarted.reactor.layout.active_context_for_test(right_space())
    );
}

/// R26. In `per_screen` scope, focus from outside switches the screen the
/// window was last shown on, not the focused screen.
#[test]
fn r26_focus_from_outside_switches_the_windows_screen() {
    let mut s = per_screen_setup();
    let left_member = wid(1);
    let c = s.create("C", &[left_member]);
    let left_window = WindowId::new(2, 1);
    // The right screen shows C; the left screen shows a different context.
    s.focus_screen(1);
    s.switch(c);
    let d = s.create("D", &[left_member]);
    s.focus_screen(0);
    s.switch(d);
    assert_eq!(ContextKey::Named(id_of(d)), s.active_on(1));

    // The user focuses app 2's left window, which is in no context, so the
    // switch goes to Unsorted on the left screen.
    let sequence_id = s.reactor.raise_sequence;
    s.reactor.handle_event(Event::RaiseFocusSent { sequence_id });
    s.reactor.handle_event(Event::RaiseTimeout { sequence_id });
    let (raise_manager_tx, _raises) = mpsc::unbounded_channel();
    s.reactor.raise_manager_tx = raise_manager_tx;
    s.reactor.handle_event(Event::ApplicationGloballyActivated(left_window.pid));
    s.reactor.handle_event(Event::ApplicationMainWindowChanged(
        left_window.pid,
        Some(left_window),
        Quiet::No,
    ));
    s.reactor.handle_event(Event::ApplicationActivated(left_window.pid, Quiet::No));
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert_eq!(ContextKey::Unsorted, s.active_on(1));
    assert_eq!(
        ContextKey::Named(id_of(c)),
        s.active_on(2),
        "the other screen keeps its context"
    );
    assert!(
        s.tiles_on(space(), screen())
            .iter()
            .any(|(wid, frame)| { *wid == left_window && !on_right(*frame) })
    );
}

/// R37. An added window keeps showing on its screen across a Space change.
/// Only an explicit switch ends that, here a switch to another context, which
/// parks it.
#[test]
fn r37_a_space_change_keeps_an_added_window_and_a_switch_ends_it() {
    let mut s = Setup::on(vec![screen(), right()], vec![Some(space()), Some(right_space())]);
    s.reactor.handle_events(s.apps.make_app(1, make_windows(1)));
    s.reactor.handle_event(Event::StartupComplete);
    s.apps.simulate_until_quiet(&mut s.reactor);
    s.scope(Scope::PerScreen);
    let added = wid(1);
    let c = s.create("C", &[]);
    s.focus_screen(0);
    s.switch(ContextKey::Unsorted);
    assert_eq!(vec![(added, screen())], s.tiles_on(space(), screen()));

    // Adding the window to C leaves it visible until the next switch.
    s.reactor.add_window_to_context(Some(added), &ContextRef::Id(id_of(c))).unwrap();
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert!(s.reactor.contexts.is_member(c, added));
    assert!(s.reactor.added_since_switch.contains(&added));
    assert_eq!(vec![(added, screen())], s.tiles_on(space(), screen()));

    // A Space change on the left screen does not end it.
    let snapshot = on_screen(&s, &[added]);
    s.reactor.handle_event(Event::SpaceChanged(
        vec![Some(SpaceId::new(3)), Some(right_space())],
        snapshot,
    ));
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert!(s.reactor.added_since_switch.contains(&added));
    assert_eq!(
        vec![(added, rect(0., 0., 1200., 1000.))],
        s.tiles_on(SpaceId::new(3), screen())
    );

    // An explicit switch to D, which doesn't hold the window, parks it.
    let d = s.create("D", &[]);
    s.switch(d);

    assert_eq!(ContextKey::Named(id_of(d)), s.active_on(1));
    assert!(!s.reactor.added_since_switch.contains(&added));
    assert!(s.parked().contains(&added));
}

#[test]
fn r37_membership_commands_use_the_windows_screen_context() {
    let mut s = per_screen_setup();
    let left_window = wid(1);
    let c = s.create("C", &[left_window, WindowId::new(2, 1)]);
    let d = s.create("D", &[]);
    s.focus_screen(0);
    s.switch(c);
    assert_eq!(ContextKey::Everything, s.active_on(2));

    s.reactor
        .move_window_to_context(Some(left_window), &ContextRef::Id(id_of(d)))
        .unwrap();
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert!(!s.reactor.contexts.is_member(c, left_window));
    assert!(s.reactor.contexts.is_member(d, left_window));
    assert!(s.parked().contains(&left_window));

    let remaining = WindowId::new(2, 1);
    s.reactor.remove_window_from_context(Some(remaining)).unwrap();
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert!(!s.reactor.contexts.is_member(c, remaining));
    assert!(s.parked().contains(&remaining));
    assert_eq!(ContextKey::Everything, s.active_on(2));
}

#[test]
fn deleting_a_context_active_on_one_screen_shows_unsorted_there() {
    let mut s = per_screen_setup();
    let c = s.create("C", &[wid(1)]);
    s.focus_screen(0);
    s.switch(c);
    assert_eq!(c, s.active_on(1));
    assert_eq!(ContextKey::Everything, s.active_on(2));

    s.reactor.delete_context(id_of(c)).unwrap();
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert_eq!(ContextKey::Unsorted, s.active_on(1));
    assert_eq!(ContextKey::Everything, s.active_on(2));
    assert!(!s.parked().contains(&wid(1)));
}

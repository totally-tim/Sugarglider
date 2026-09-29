// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use test_log::test;

use super::*;
use crate::model::native_tabs::TabGroup;

pub(super) fn observe(s: &mut Setup, id: u64, selected: WindowId, members: &[Option<WindowId>]) {
    s.reactor.handle_event(Event::NativeTabsChanged {
        window: selected,
        group: Some(TabGroup {
            id,
            selected,
            members: members.to_vec(),
        }),
    });
}

#[test]
fn overlapping_windows_remain_independent_members_and_tiles() {
    let mut s = Setup::on(vec![screen()], vec![Some(space())]);
    let frame = rect(900., 0., 300., 1000.);
    let windows = (1..=3).map(|i| WindowInfo { frame, ..make_window(i) }).collect();
    s.reactor
        .handle_events(s.apps.make_app_with_opts(1, windows, Some(wid(1)), false));
    s.reactor.handle_event(Event::StartupComplete);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(s.reactor.tabs_of(wid(1)), [wid(1)]);
    assert_eq!(s.reactor.group_windows(&[wid(1)]), [wid(1)]);
    assert_eq!(s.tiles().len(), 3);
    let c = s.create("Only first", &[wid(1)]);
    s.switch(c);
    assert_eq!(s.parked(), [wid(2), wid(3)]);
}

#[test]
fn a_refused_frame_target_restores_everything_without_inventing_a_tab_group() {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1)]);
    s.switch(c);
    s.reactor.handle_event(Event::FrameTargetUnavailable(wid(1)));
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(s.reactor.contexts.active(), ContextKey::Everything);
    assert!(s.parked().is_empty());
    assert!(s.reactor.unavailable_tabs.is_empty());
    assert!(!s.reactor.unidentified_native_tabs());
    s.switch(c);
    assert_eq!(s.reactor.contexts.active(), c);
}

#[test]
fn a_late_frame_refusal_for_an_inactive_visible_tab_keeps_the_context() {
    let mut s = Setup::new(3);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), Some(wid(2))]);
    let c = s.create("C", &[wid(1)]);
    s.switch(c);
    observe(&mut s, 10, wid(2), &[Some(wid(1)), Some(wid(2))]);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(s.reactor.contexts.active(), c);
    assert!(
        s.reactor.window_on_screen(wid(1)),
        "WindowServer has not caught up"
    );
    s.reactor.handle_event(Event::FrameTargetUnavailable(wid(1)));
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(s.reactor.contexts.active(), c);
    assert_eq!(s.parked(), [wid(3)]);
    assert_eq!(
        s.tiles().iter().map(|(wid, _)| *wid).collect::<Vec<_>>(),
        [wid(2)]
    );
}

#[test]
fn frame_refusals_outside_managed_context_windows_do_not_change_state() {
    for case in ["everything", "disabled", "offscreen", "untracked"] {
        let mut s = Setup::new(2);
        let c = s.create("C", &[wid(1)]);
        if case != "everything" {
            s.switch(c);
        }
        match case {
            "disabled" => s.reactor.handle_event(Event::ConfigChanged(config(false))),
            "offscreen" => {
                let wsid = s.reactor.windows[&wid(1)].window_server_id.unwrap();
                s.reactor.visible_windows.remove(&wsid);
            }
            "untracked" => {
                let wsid = s.reactor.windows[&wid(1)].window_server_id.unwrap();
                s.reactor.window_server_info.get_mut(&wsid).unwrap().layer = 3;
            }
            _ => (),
        }
        s.apps.simulate_until_quiet(&mut s.reactor);
        let before = (s.reactor.contexts.active(), s.parked());
        s.reactor.handle_event(Event::FrameTargetUnavailable(wid(1)));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!((s.reactor.contexts.active(), s.parked()), before, "{case}");
        assert!(s.reactor.unavailable_tabs.is_empty());
    }
}

#[test]
fn a_valid_native_snapshot_clears_old_member_read_failures() {
    let mut s = Setup::new(3);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), Some(wid(2))]);
    let c = s.create("C", &[wid(1)]);
    s.reactor.handle_event(Event::NativeTabsUnavailable(wid(1)));
    observe(&mut s, 10, wid(2), &[Some(wid(1)), Some(wid(2))]);
    assert!(s.reactor.unavailable_tabs.is_empty());
    assert!(!s.reactor.unidentified_native_tabs());
    s.switch(c);
    assert_eq!(s.reactor.contexts.active(), c);
}

#[test]
fn native_tab_selection_keeps_the_group_tile_with_contexts_disabled() {
    let mut s = Setup::new(3);
    s.reactor.handle_event(Event::ConfigChanged(config(false)));
    observe(&mut s, 10, wid(1), &[Some(wid(1)), None]);
    observe(&mut s, 10, wid(2), &[Some(wid(1)), Some(wid(2))]);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(
        s.tiles().iter().map(|(wid, _)| *wid).collect::<Vec<_>>(),
        [wid(2), wid(3)]
    );
    let mut expected: Vec<_> = s
        .tiles()
        .into_iter()
        .map(|(window, frame)| (if window == wid(2) { wid(1) } else { window }, frame))
        .collect();
    expected.sort_by_key(|(wid, _)| *wid);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), Some(wid(2))]);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(s.tiles(), expected);
    assert!(s.parked().is_empty());
    assert!(s.reactor.contexts.contexts().is_empty());
}

#[test]
fn unidentified_tabs_restore_everything_and_reject_switches() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1)]);
    s.switch(c);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), None]);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(s.reactor.contexts.active(), ContextKey::Everything);
    assert!(s.parked().is_empty());
    assert!(
        s.reactor
            .switch_context_on(None, c, None)
            .unwrap_err()
            .contains("Select each tab once")
    );
    assert_eq!(s.reactor.contexts.active(), ContextKey::Everything);
}

#[test]
fn startup_with_unidentified_tabs_keeps_everything_until_an_explicit_switch() {
    let mut s = Setup::on(vec![screen()], vec![Some(space())]);
    let c = s.create("Saved", &[]);
    s.reactor.contexts.switch_to(c).unwrap();
    s.reactor.save_contexts();
    observe(&mut s, 10, wid(1), &[Some(wid(1)), None]);
    s.reactor.handle_events(s.apps.make_app(1, make_windows(3)));
    s.reactor.handle_event(Event::StartupComplete);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(s.reactor.contexts.active(), ContextKey::Everything);
    assert!(s.parked().is_empty());
    observe(&mut s, 10, wid(2), &[Some(wid(1)), Some(wid(2))]);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(s.reactor.contexts.active(), ContextKey::Everything);
    assert!(s.parked().is_empty());
    s.switch(c);
    assert_eq!(s.reactor.contexts.active(), c);
}

#[test]
fn selection_binds_inactive_tabs_without_geometry_or_title_matching() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[wid(2)]);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), None]);
    observe(&mut s, 10, wid(2), &[Some(wid(1)), Some(wid(2))]);
    assert_eq!(s.reactor.contexts.contexts_of(wid(2)), [id_of(c)]);
    assert!(!s.reactor.contexts.contexts_of(wid(2)).contains(&id_of(d)));
    assert_eq!(s.reactor.membership_window(wid(1)), wid(2));
    assert_eq!(s.reactor.tabs_of(wid(2)), [wid(2), wid(1)]);
    s.switch(c);
    assert_eq!(s.parked(), [wid(3)]);
    assert_eq!(
        s.tiles().iter().map(|(wid, _)| *wid).collect::<Vec<_>>(),
        [wid(2)]
    );
}

#[test]
fn active_tab_close_keeps_group_membership_for_its_successor() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1)]);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), None]);
    observe(&mut s, 10, wid(2), &[Some(wid(1)), Some(wid(2))]);
    s.reactor.handle_event(Event::WindowDestroyed(wid(2)));
    observe(&mut s, 10, wid(1), &[Some(wid(1))]);
    assert_eq!(s.reactor.contexts.contexts_of(wid(1)), [id_of(c)]);
    assert_eq!(s.reactor.tabs_of(wid(1)), [wid(1)]);
    assert!(!s.reactor.unidentified_native_tabs());
}

#[test]
fn closing_the_only_identified_tab_keeps_membership_for_an_unvisited_successor() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1)]);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), None]);
    s.reactor.handle_event(Event::WindowDestroyed(wid(1)));
    observe(&mut s, 10, wid(2), &[Some(wid(2))]);
    assert_eq!(s.reactor.contexts.contexts_of(wid(2)), [id_of(c)]);
}

#[test]
fn known_tabs_inherit_policy_before_the_last_tab_is_identified() {
    let mut s = Setup::new(3);
    let c = s.create("Original", &[wid(1)]);
    s.create("Different", &[wid(2)]);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), None, None]);
    observe(&mut s, 10, wid(2), &[Some(wid(1)), Some(wid(2)), None]);
    assert_eq!(s.reactor.contexts.contexts_of(wid(2)), [id_of(c)]);
    s.close(wid(1));
    observe(&mut s, 10, wid(3), &[Some(wid(2)), Some(wid(3))]);
    assert_eq!(s.reactor.contexts.contexts_of(wid(2)), [id_of(c)]);
    assert_eq!(s.reactor.contexts.contexts_of(wid(3)), [id_of(c)]);
}

#[test]
fn detaching_a_tab_leaves_two_independent_memberships() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[]);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), None]);
    observe(&mut s, 10, wid(2), &[Some(wid(1)), Some(wid(2))]);
    s.reactor.handle_event(Event::NativeTabsChanged { window: wid(2), group: None });
    observe(&mut s, 10, wid(1), &[Some(wid(1))]);
    s.reactor
        .add_window_to_context(Some(wid(2)), &ContextRef::Id(id_of(d)))
        .unwrap();
    assert_eq!(s.reactor.contexts.contexts_of(wid(1)), [id_of(c)]);
    assert_eq!(s.reactor.contexts.contexts_of(wid(2)), [id_of(c), id_of(d)]);
}

#[test]
fn closing_an_inactive_identified_tab_does_not_leave_an_unknown_slot() {
    let mut s = Setup::new(3);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), Some(wid(2)), Some(wid(3))]);
    let c = s.create("C", &[wid(1)]);
    s.close(wid(3));
    s.reactor.handle_event(Event::WindowsOnScreenUpdated {
        pid: Some(1),
        on_screen: on_screen(&s, &[wid(1), wid(2)]),
    });
    assert_eq!(s.reactor.native_group(wid(1)).unwrap().snapshot.members.len(), 2);
    assert!(!s.reactor.unidentified_native_tabs());
    s.switch(c);
    assert_eq!(s.reactor.contexts.active(), c);
}

#[test]
fn a_destroyed_selected_tab_cannot_suppress_the_surviving_window() {
    let mut s = Setup::new(2);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), Some(wid(2))]);
    s.close(wid(1));
    assert_eq!(s.reactor.membership_window(wid(2)), wid(2));
    assert!(s.reactor.reaches_layout(space(), wid(2)));
    s.close(wid(2));
    assert!(s.reactor.native_tabs.is_empty());
    assert!(s.reactor.layout.all_windows().is_empty());
}

#[test]
fn inactive_tabs_are_rejected_by_the_shared_layout_predicate() {
    let mut s = Setup::new(2);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), Some(wid(2))]);
    assert!(s.reactor.reaches_layout(space(), wid(1)));
    assert!(!s.reactor.reaches_layout(space(), wid(2)));
}

#[test]
fn a_parked_group_recovers_through_its_new_selected_tab() {
    for floating in [false, true] {
        let mut s = Setup::new(3);
        observe(&mut s, 10, wid(1), &[Some(wid(1)), Some(wid(2))]);
        if floating {
            s.reactor.send_layout_event(LayoutEvent::WindowFocused(vec![space()], wid(1)));
            s.reactor.handle_event(Event::Command(Command::Layout(
                LayoutCommand::ToggleWindowFloating,
            )));
        }
        s.apps.simulate_until_quiet(&mut s.reactor);
        let before = s.frame(wid(1));
        let d = s.create("Other", &[wid(3)]);
        s.switch(d);
        assert_eq!(s.parked(), [wid(1)]);
        assert_eq!(s.journal_on_disk().len(), 2);
        let old_frame = s.frame(wid(1));
        let old_txid = s.reactor.windows[&wid(1)].last_sent_txid;
        s.apps.windows.get_mut(&wid(2)).unwrap().frame = old_frame;
        s.reactor.windows.get_mut(&wid(2)).unwrap().frame_monotonic = old_frame;
        observe(&mut s, 10, wid(2), &[Some(wid(1)), Some(wid(2))]);
        assert_eq!(s.reactor.contexts.active(), ContextKey::Everything);
        let requests = s.apps.requests();
        assert!(frame_writes(&requests, wid(1)).is_empty());
        assert!(!frame_writes(&requests, wid(2)).is_empty());
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            old_frame,
            old_txid,
            Requested(true),
            None,
        ));
        assert!(
            !s.journal_on_disk().is_empty(),
            "inactive echoes cannot confirm group restoration"
        );
        answer(&mut s, requests);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert!(s.journal_on_disk().is_empty());
        if floating {
            assert_eq!(s.frame(wid(2)), before);
        }
        assert!(s.parked().is_empty());
    }
}

#[test]
fn closing_a_parked_selected_tab_keeps_a_recovery_entry_for_its_successor() {
    let mut s = Setup::new(3);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), Some(wid(2))]);
    let d = s.create("Other", &[wid(3)]);
    s.switch(d);
    let parked = s.frame(wid(1));
    s.close(wid(1));
    assert_eq!(s.journal_on_disk().len(), 1);
    s.apps.windows.get_mut(&wid(2)).unwrap().frame = parked;
    s.reactor.windows.get_mut(&wid(2)).unwrap().frame_monotonic = parked;
    observe(&mut s, 10, wid(2), &[Some(wid(2))]);
    let requests = s.apps.requests();
    assert!(frame_writes(&requests, wid(1)).is_empty());
    assert!(!frame_writes(&requests, wid(2)).is_empty());
    answer(&mut s, requests);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(s.reactor.contexts.active(), ContextKey::Everything);
    assert!(s.journal_on_disk().is_empty());
}

#[test]
fn a_journal_failure_during_a_parked_merge_preserves_recovery_and_restores_the_selected_tab() {
    let mut s = Setup::new(4);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), Some(wid(2))]);
    let other = s.create("Other", &[wid(4)]);
    s.switch(other);
    let journal = s.journal_on_disk();
    let parked = s.frame(wid(1));
    s.apps.windows.get_mut(&wid(3)).unwrap().frame = parked;
    s.reactor.windows.get_mut(&wid(3)).unwrap().frame_monotonic = parked;
    let failing = FailingWrites::start(s.dir.path());
    observe(&mut s, 10, wid(3), &[Some(wid(1)), Some(wid(2)), Some(wid(3))]);
    let requests = s.apps.requests();
    drop(failing);
    assert_eq!(s.journal_on_disk(), journal);
    assert_eq!(s.reactor.contexts.active(), ContextKey::Everything);
    assert!(frame_writes(&requests, wid(1)).is_empty());
    assert!(frame_writes(&requests, wid(2)).is_empty());
    assert!(!frame_writes(&requests, wid(3)).is_empty());
    answer(&mut s, requests);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert!(s.parked().is_empty());
    assert!(s.journal_on_disk().is_empty());
}

#[test]
fn repark_updates_every_tab_recovery_frame_before_a_display_disappears() {
    let mut s = Setup::on(
        vec![screen(), right()],
        vec![Some(space()), Some(SpaceId::new(2))],
    );
    let windows = (1..=2)
        .map(|i| WindowInfo {
            frame: rect(1300., 100., 400., 400.),
            ..make_window(i)
        })
        .collect();
    s.reactor.handle_events(s.apps.make_app(1, windows));
    s.reactor.handle_event(Event::StartupComplete);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), Some(wid(2))]);
    s.apps.simulate_until_quiet(&mut s.reactor);
    let empty = s.create("Empty", &[]);
    s.switch(empty);
    assert_eq!(s.parked(), [wid(1)]);
    assert!(s.journal_on_disk().iter().all(|entry| entry.frame.x >= 1200.));
    s.reactor.handle_event(screens(vec![screen()], vec![Some(space())]));
    let journal = s.journal_on_disk();
    assert_eq!(journal.len(), 2);
    assert!(journal.iter().all(|entry| entry.frame.x < 1200.));
    assert_eq!(journal[0].frame, journal[1].frame);
    let requests = s.apps.requests();
    assert!(frame_writes(&requests, wid(2)).is_empty());
    assert!(!frame_writes(&requests, wid(1)).is_empty());
}

#[test]
fn invalid_cross_process_group_fails_closed_without_replacing_known_identity() {
    let mut s = Setup::new(3);
    observe(&mut s, 10, wid(1), &[Some(wid(1))]);
    observe(&mut s, 10, wid(1), &[Some(wid(1)), Some(WindowId::new(2, 1))]);
    assert_eq!(s.reactor.tabs_of(wid(1)), [wid(1)]);
    assert!(s.reactor.unidentified_native_tabs());
}

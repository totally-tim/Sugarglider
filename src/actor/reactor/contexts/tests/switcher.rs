// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reactor tests for the switcher's commands: the ones that carry the
//! window they act on, create a context from a window list, edit a context's
//! members, rename it, number it, and delete it. The JSON contract they come
//! from is `docs/specs/contexts-switcher-contract.md`.

use test_log::test;

use super::*;
use crate::actor::reactor::RecordRef;
use crate::actor::reactor::testing::WindowState;

/// App `wid.pid` opens a window, which reaches the reactor as a new window
/// does: `WindowCreated`, then the window server's list, which names the
/// windows in `listed` and the new one, then `WindowBecameVisible`.
fn open_window(s: &mut Setup, wid: WindowId, info: WindowInfo, listed: &[WindowId]) {
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
    s.apps.simulate_until_quiet(&mut s.reactor);
}

/// Gives the window the focus, as a switch's own raise does.
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

/// Adds to `key` an empty record of a window of app `app` that is gone, as
/// an earlier run of the app leaves it.
fn empty_record(s: &mut Setup, key: ContextKey, app: i32, title: &str) {
    let gone = WindowId::new(90, 1);
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

/// R37. The switcher's add and move carry the target window, which is
/// not the window that has focus when they arrive: the panel has key focus
/// while it is open. A command with no window still acts on the focused one,
/// which is what a key binding uses.
#[test]
fn the_switchers_window_commands_act_on_the_window_they_carry() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1), wid(2)]);
    let d = s.create("D", &[]);
    let e = s.create("E", &[]);
    s.switch(c);
    focus_quietly(&mut s, wid(2));

    // No window in the command: the focused window counts, which is what a
    // key binding uses.
    run(
        &mut s,
        ContextCommand::AddWindow {
            window: None,
            context: ContextRef::Id(id_of(d)),
        },
    );
    assert_eq!(vec![id_of(c), id_of(d)], s.reactor.contexts.contexts_of(wid(2)));

    // With a window: that window counts, not the focused one.
    run(
        &mut s,
        ContextCommand::AddWindow {
            window: Some(wid(1)),
            context: ContextRef::Id(id_of(d)),
        },
    );
    assert_eq!(vec![id_of(c), id_of(d)], s.reactor.contexts.contexts_of(wid(1)));
    assert!(!s.reactor.contexts.is_member(e, wid(1)));

    run(&mut s, ContextCommand::TogglePinned { window: Some(wid(1)) });
    assert!(s.reactor.contexts.is_pinned(wid(1)));
    run(&mut s, ContextCommand::TogglePinned { window: Some(wid(1)) });
    assert!(!s.reactor.contexts.is_pinned(wid(1)));

    // Moving takes the window out of the active context.
    run(
        &mut s,
        ContextCommand::MoveWindow {
            window: Some(wid(1)),
            context: ContextRef::Id(id_of(e)),
        },
    );
    assert_eq!(vec![id_of(d), id_of(e)], s.reactor.contexts.contexts_of(wid(1)));
    assert!(!s.reactor.contexts.is_member(c, wid(1)));
}

/// R36, R3. The switcher's create makes a context whose members are
/// exactly the windows it lists: a window resolves to its native tab group,
/// and a pinned window gets no record, because it is a member of every
/// context already.
#[test]
fn the_switchers_create_makes_a_context_from_the_windows_it_carries() {
    let mut s = Setup::new(2);
    let c = s.create("C", &[wid(1), wid(2)]);
    s.create("D", &[]);
    s.switch(c);
    focus_quietly(&mut s, wid(1));
    let tab = WindowInfo {
        frame: s.frame(wid(1)),
        ..make_window(3)
    };
    super::native_tabs::observe(&mut s, 1, wid(1), &[Some(wid(1))]);
    super::native_tabs::observe(&mut s, 1, wid(3), &[Some(wid(1)), Some(wid(3))]);
    open_window(&mut s, wid(3), tab, &[wid(1), wid(2)]);
    assert!(s.reactor.contexts.is_member(c, wid(3)));
    s.reactor.contexts.pin(&s.desc(wid(2)));

    run(
        &mut s,
        ContextCommand::CreateContextFromWindows {
            name: "E".into(),
            windows: vec![wid(3), wid(2)],
        },
    );
    s.apps.simulate_until_quiet(&mut s.reactor);

    let e = ContextKey::Named(s.reactor.contexts.by_name("E").unwrap().id);
    assert_eq!(ContextKey::Named(id_of(e)), s.reactor.contexts.active());
    // Window 3's tab group is window 1 and window 3; window 2 is pinned.
    let mut members: Vec<WindowId> = s
        .reactor
        .contexts
        .get(id_of(e))
        .unwrap()
        .members
        .iter()
        .filter_map(|record| record.window())
        .collect();
    members.sort();
    assert_eq!(vec![wid(1), wid(3)], members);
    let mut saved = saved_members(&s, e);
    saved.sort();
    assert_eq!(vec!["Window1", "Window3"], saved);
}

/// R37, R23. The switcher's edit removes a gone record, removes a window
/// at once, and adds a window for the next switch.
#[test]
fn the_switchers_edit_changes_a_contexts_members_in_order() {
    let mut s = Setup::new(3);
    let c = s.create("C", &[wid(1), wid(2)]);
    s.switch(c);
    empty_record(&mut s, c, 2, "Gone");

    run(
        &mut s,
        ContextCommand::EditContext {
            context: ContextRef::Id(id_of(c)),
            add: vec![wid(3)],
            remove: vec![wid(2)],
            remove_records: vec![RecordRef {
                record: 2,
                app: "TestApp2".into(),
                title: "Gone".into(),
            }],
        },
    );
    s.apps.simulate_until_quiet(&mut s.reactor);

    assert_eq!(vec!["Window1", "Window3"], saved_members(&s, c));
    assert_eq!(vec![id_of(c)], s.reactor.contexts.contexts_of(wid(3)));
    assert!(s.reactor.contexts.contexts_of(wid(2)).is_empty());
    assert_eq!(vec![wid(2), wid(3)], s.parked());

    // A record that no longer matches is skipped, and the edit still runs.
    run(
        &mut s,
        ContextCommand::EditContext {
            context: ContextRef::Id(id_of(c)),
            add: vec![],
            remove: vec![],
            remove_records: vec![RecordRef {
                record: 0,
                app: "Other".into(),
                title: "Other".into(),
            }],
        },
    );
    assert_eq!(vec!["Window1", "Window3"], saved_members(&s, c));
    assert!(s.reactor.contexts.is_unsorted(wid(2)));
}

/// R4, R5, R6. The switcher's rename, set_number, and delete follow the
/// model's rules, and a name that is taken is refused with the model's
/// message.
#[test]
fn the_switchers_rename_number_and_delete_follow_the_rules() {
    let mut s = Setup::new(1);
    let c = s.create("C", &[wid(1)]);
    let d = s.create("D", &[]);
    s.switch(c);

    run(
        &mut s,
        ContextCommand::RenameContext {
            context: ContextRef::Id(id_of(d)),
            name: "Client work".into(),
        },
    );
    assert_eq!("Client work", s.reactor.contexts.get(id_of(d)).unwrap().name);
    let names: Vec<&str> = s.reactor.contexts.contexts().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(vec!["C", "Client work"], names);

    run(
        &mut s,
        ContextCommand::SetContextNumber {
            context: ContextRef::Id(id_of(d)),
            number: 2,
        },
    );
    assert_eq!(Some(2), s.reactor.contexts.get(id_of(d)).unwrap().number);

    run(&mut s, ContextCommand::DeleteContext(ContextRef::Id(id_of(d))));
    assert!(s.reactor.contexts.get(id_of(d)).is_none());
    assert_eq!(ContextKey::Named(id_of(c)), s.reactor.contexts.active());

    // The rules refuse a name another context holds, a number outside 1 to
    // 9, and the id of the context that was just deleted.
    let e = s.create("E", &[]);
    for command in [
        ContextCommand::RenameContext {
            context: ContextRef::Id(id_of(c)),
            name: "E".into(),
        },
        ContextCommand::SetContextNumber {
            context: ContextRef::Id(id_of(c)),
            number: 0,
        },
        ContextCommand::DeleteContext(ContextRef::Id(id_of(d))),
    ] {
        let result = s.reactor.run_context_command(command);
        assert!(result.is_err(), "the command should be refused");
    }
    assert_eq!("C", s.reactor.contexts.get(id_of(c)).unwrap().name);
    assert_eq!(Some(1), s.reactor.contexts.get(id_of(c)).unwrap().number);
    assert!(s.reactor.contexts.get(id_of(e)).is_some());
}

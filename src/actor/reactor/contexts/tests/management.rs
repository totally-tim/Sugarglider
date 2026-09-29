// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reactor tests for the commands that change contexts: rename, number,
//! delete, edit, and forget.

use test_log::test;

use super::membership::{focus_quietly, open_window};
use super::*;
use crate::actor::contexts_snapshot::{CommandResult, RequestId};
use crate::actor::reactor::RecordRef;
use crate::actor::server::{ContextRequest, Response, answer_context_request};
use crate::model::contexts::Contexts;
use crate::sys::app::WindowInfo;

/// Sends a context command from the command line with request id
/// `request`, and lets the apps answer.
fn request(s: &mut Setup, request: u64, command: ContextCommand) {
    s.reactor
        .handle_event(Event::ContextCommandRequested(RequestId(request), command));
    s.apps.simulate_until_quiet(&mut s.reactor);
}

/// The server's reply when the command line asks for the result of
/// `request`, from the snapshot the reactor published last.
fn result_of(s: &Setup, request: u64) -> Response {
    let snapshot = s.reactor.published_contexts.as_deref();
    answer_context_request(ContextRequest::Result(RequestId(request)), snapshot).0
}

fn ran(request: u64) -> CommandResult {
    CommandResult {
        request: RequestId(request),
        error: None,
    }
}

fn failed(request: u64, reason: &str) -> CommandResult {
    CommandResult {
        request: RequestId(request),
        error: Some(reason.to_string()),
    }
}

/// The results the reactor published for the commands sent so far.
fn results(s: &Setup) -> Vec<CommandResult> {
    s.reactor.published_contexts.as_deref().unwrap().results.clone()
}

/// The context's open member windows.
fn open_members(s: &Setup, id: ContextId) -> Vec<WindowId> {
    s.reactor
        .contexts
        .get(id)
        .unwrap()
        .members
        .iter()
        .filter_map(|member| member.window())
        .collect()
}

/// The contexts as `contexts.json` holds them.
fn saved(s: &Setup) -> Contexts {
    match ContextsStore::new(s.dir.path().join("contexts.json")).load(SystemTime::now()) {
        Loaded::Read { contexts, .. } => contexts,
        other => panic!("{other:?}"),
    }
}

/// A context id that no context of the test gets from `create`.
fn context_id(id: u32) -> ContextId {
    serde_json::from_value(serde_json::json!(id)).unwrap()
}

/// Names a member record by its index, app, and title, as a client does
/// after it reads the snapshot.
fn record_ref(record: usize, app: &str, title: &str) -> RecordRef {
    RecordRef {
        record,
        app: app.into(),
        title: title.into(),
    }
}

/// R4. `rename_context` trims the name and refuses an empty, reserved, or
/// taken one with the reason the command line prints. A name that differs
/// from the context's own only in case or accents is not taken.
#[test]
fn rename_context_changes_the_name_and_refuses_a_bad_one() {
    let mut s = Setup::new(2);
    let comms = id_of(s.create("Comms", &[wid(1)]));
    let work = id_of(s.create("Work", &[wid(2)]));
    let rename = |context: ContextRef, name: &str| ContextCommand::RenameContext {
        context,
        name: name.into(),
    };

    request(&mut s, 1, rename(ContextRef::Number(1), "  Client work  "));
    request(&mut s, 2, rename(ContextRef::Name("work".into()), "CLIENT WÖRK"));
    request(&mut s, 3, rename(ContextRef::Id(work), "Everything"));
    request(&mut s, 4, rename(ContextRef::Id(work), "  "));
    request(&mut s, 5, rename(ContextRef::Name("nothing".into()), "X"));
    request(&mut s, 6, rename(ContextRef::Id(work), "work"));

    assert_eq!("Client work", s.reactor.contexts.get(comms).unwrap().name);
    assert_eq!("work", s.reactor.contexts.get(work).unwrap().name);
    assert_eq!(
        vec![
            ran(1),
            failed(2, "A context named \"Client work\" already exists"),
            failed(3, "\"Everything\" is a reserved name"),
            failed(4, "A context name can't be empty"),
            failed(5, "No context matches \"nothing\""),
            ran(6),
        ],
        results(&s)
    );
    assert_eq!(Response::Success, result_of(&s, 1));
    assert_eq!(
        Response::Error("A context named \"Client work\" already exists".into()),
        result_of(&s, 2)
    );
    assert_eq!("Client work", saved(&s).get(comms).unwrap().name);
    assert_eq!("work", saved(&s).get(work).unwrap().name);
}

/// R5. `set_context_number` gives the context its query names a number from
/// 1 to 9 and takes the number from the context that had it. A number
/// outside 1 to 9 is refused, and Everything and Unsorted can't be numbered.
#[test]
fn set_context_number_takes_the_number_from_its_holder() {
    let mut s = Setup::new(3);
    let comms = id_of(s.create("Comms", &[wid(1)]));
    let work = id_of(s.create("Work", &[wid(2)]));
    let number =
        |context: ContextRef, number: u8| ContextCommand::SetContextNumber { context, number };

    request(&mut s, 1, number(ContextRef::Name("Work".into()), 1));
    request(&mut s, 2, number(ContextRef::Id(work), 0));
    request(&mut s, 3, number(ContextRef::Id(work), 10));
    request(&mut s, 4, number(ContextRef::Name("Unsorted".into()), 3));
    request(&mut s, 5, number(ContextRef::Name("nothing".into()), 3));

    assert_eq!(Some(1), s.reactor.contexts.get(work).unwrap().number);
    assert_eq!(None, s.reactor.contexts.get(comms).unwrap().number);
    assert_eq!(
        vec![
            ran(1),
            failed(2, "Context numbers go from 1 to 9, not 0"),
            failed(3, "Context numbers go from 1 to 9, not 10"),
            failed(4, "Only a named context can be numbered"),
            failed(5, "No context matches \"nothing\""),
        ],
        results(&s)
    );
    assert_eq!(Some(1), saved(&s).get(work).unwrap().number);
}

/// R6, L2. Deleting a context never touches its windows. The windows that
/// only it held become unsorted, deleting the active one shows Unsorted,
/// and its layouts go. Deleting an inactive one changes nothing that shows.
#[test]
fn deleting_a_context_keeps_its_windows_and_drops_its_layouts() {
    let mut s = Setup::new(3);
    let a = id_of(s.create("A", &[wid(1)]));
    let b = id_of(s.create("B", &[wid(2)]));
    // Both contexts get a layout: B while it is active, then A again.
    s.switch(ContextKey::Named(b));
    s.switch(ContextKey::Named(a));
    assert_eq!(vec![wid(2), wid(3)], s.parked());
    assert!(s.reactor.layout.context_ids().any(|id| id == a));
    assert!(s.reactor.layout.context_ids().any(|id| id == b));

    request(
        &mut s,
        1,
        ContextCommand::DeleteContext(ContextRef::Name("A".into())),
    );

    assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
    assert!(s.reactor.contexts.get(a).is_none());
    assert!(s.reactor.contexts.is_unsorted(wid(1)));
    assert_eq!(vec![wid(2)], s.parked());
    assert!(!s.reactor.layout.context_ids().any(|id| id == a));
    assert!(s.reactor.layout.context_ids().any(|id| id == b));

    request(
        &mut s,
        2,
        ContextCommand::DeleteContext(ContextRef::Name("Unsorted".into())),
    );
    request(
        &mut s,
        3,
        ContextCommand::DeleteContext(ContextRef::Name("nothing".into())),
    );
    request(
        &mut s,
        4,
        ContextCommand::DeleteContext(ContextRef::Id(context_id(999))),
    );
    // B is inactive, so nothing that shows changes.
    request(&mut s, 5, ContextCommand::DeleteContext(ContextRef::Number(2)));

    assert_eq!(
        vec![
            ran(1),
            failed(2, "Only a named context can be deleted"),
            failed(3, "No context matches \"nothing\""),
            failed(4, "No such context"),
            ran(5),
        ],
        results(&s)
    );
    assert!(s.reactor.contexts.get(b).is_none());
    assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
    assert!(s.parked().is_empty());
    assert!(s.reactor.contexts.contexts().is_empty());
    assert!(saved(&s).contexts().is_empty());
}

/// R37. The edit removes the windows in `remove` at once, parks the ones
/// that no longer show, and adds the windows in `add` for the next switch,
/// where a parked one returns to its place.
#[test]
fn an_edit_removes_windows_at_once_and_adds_them_for_the_next_switch() {
    let mut s = Setup::new(4);
    let c = ContextKey::Named(id_of(s.create("C", &[wid(1), wid(2)])));
    s.switch(c);
    assert_eq!(vec![wid(3), wid(4)], s.parked());

    request(
        &mut s,
        1,
        ContextCommand::EditContextMembers {
            context: ContextRef::Id(id_of(c)),
            add: vec![wid(4)],
            remove: vec![wid(2)],
            remove_records: vec![],
        },
    );

    assert_eq!(Response::Success, result_of(&s, 1));
    assert_eq!(vec![wid(2), wid(3), wid(4)], s.parked());
    assert_eq!(vec![wid(1), wid(4)], open_members(&s, id_of(c)));
    let saved_c = saved(&s);
    assert_eq!(2, saved_c.get(id_of(c)).unwrap().members.len());

    // The window added for the next switch returns to its tile, and the
    // removed one stays parked.
    s.switch(c);
    assert_eq!(vec![wid(2), wid(3)], s.parked());
    assert_eq!(vec![wid(1), wid(4)], open_members(&s, id_of(c)));
    assert_eq!(
        vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(4), rect(600., 0., 600., 1000.)),
        ],
        s.tiles()
    );
    assert_eq!(s.tiles(), s.frames(&[wid(1), wid(4)]));
}

/// R23. The edit removes a record only while its index still names a record
/// with no open window and the same app and title. A record of an open
/// window, a record that changed, and an index off the end are skipped, and
/// the rest of the edit runs.
#[test]
fn an_edit_removes_a_record_only_while_it_still_matches() {
    let mut s = Setup::new(3);
    let c = id_of(s.create("C", &[wid(1), wid(2)]));
    s.switch(ContextKey::Named(c));
    // Window 2 closes, so its record is pending.
    s.apps.windows.remove(&wid(2));
    s.reactor.handle_event(Event::WindowDestroyed(wid(2)));
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(vec![wid(1)], open_members(&s, c));

    request(
        &mut s,
        1,
        ContextCommand::EditContextMembers {
            context: ContextRef::Id(c),
            add: vec![],
            remove: vec![],
            remove_records: vec![
                record_ref(9, "TestApp1", "Window2"),
                record_ref(1, "TestApp1", "Another title"),
                record_ref(1, "TestApp1", "Window2"),
                record_ref(0, "TestApp1", "Window1"),
            ],
        },
    );

    assert_eq!(Response::Success, result_of(&s, 1));
    let members = &s.reactor.contexts.get(c).unwrap().members;
    assert_eq!(1, members.len());
    assert_eq!(Some(wid(1)), members[0].window());
    assert_eq!(vec![wid(1)], open_members(&s, c));
    assert_eq!(1, saved(&s).get(c).unwrap().members.len());
}

/// R3. A pinned window is a member of every context already, so an edit
/// that names it in `add` or `remove` gives it no record, leaves the
/// records as they are, and doesn't park it.
#[test]
fn an_edit_leaves_a_pinned_window_alone() {
    let mut s = Setup::new(2);
    let c = id_of(s.create("C", &[wid(1)]));
    let pinned = s.desc(wid(2));
    s.reactor.contexts.pin(&pinned);
    s.switch(ContextKey::Named(c));
    assert!(s.parked().is_empty());

    request(
        &mut s,
        1,
        ContextCommand::EditContextMembers {
            context: ContextRef::Id(c),
            add: vec![wid(2)],
            remove: vec![wid(2)],
            remove_records: vec![],
        },
    );

    assert_eq!(Response::Success, result_of(&s, 1));
    let members = &s.reactor.contexts.get(c).unwrap().members;
    assert_eq!(1, members.len());
    assert_eq!(Some(wid(1)), members[0].window());
    assert!(s.reactor.contexts.is_pinned(wid(2)));
    assert!(!s.reactor.added_since_switch.contains(&wid(2)));
    assert!(s.parked().is_empty());
}

/// R23. `remove_record` removes the member record at the index, and
/// refuses a record whose window is still open. Everything and Unsorted
/// have no records to remove.
#[test]
fn remove_record_removes_the_record_at_the_index() {
    let mut s = Setup::new(3);
    let c = id_of(s.create("C", &[wid(1), wid(2)]));
    s.switch(ContextKey::Named(c));
    s.apps.windows.remove(&wid(2));
    s.reactor.handle_event(Event::WindowDestroyed(wid(2)));
    s.apps.simulate_until_quiet(&mut s.reactor);

    request(
        &mut s,
        1,
        ContextCommand::RemoveRecord {
            context: ContextRef::Name("C".into()),
            record: record_ref(1, "TestApp1", "Window2"),
        },
    );
    request(
        &mut s,
        2,
        ContextCommand::RemoveRecord {
            context: ContextRef::Id(c),
            record: record_ref(0, "TestApp1", "Window1"),
        },
    );
    request(
        &mut s,
        3,
        ContextCommand::RemoveRecord {
            context: ContextRef::Id(c),
            record: record_ref(5, "TestApp1", "Window9"),
        },
    );
    request(
        &mut s,
        4,
        ContextCommand::RemoveRecord {
            context: ContextRef::Name("Unsorted".into()),
            record: record_ref(0, "TestApp1", "Window1"),
        },
    );
    request(
        &mut s,
        5,
        ContextCommand::RemoveRecord {
            context: ContextRef::Name("nothing".into()),
            record: record_ref(0, "TestApp1", "Window1"),
        },
    );

    assert_eq!(
        vec![
            ran(1),
            failed(2, "The member's window is open; remove the window instead"),
            failed(3, "No such member record"),
            failed(4, "Only a named context has member records"),
            failed(5, "No context matches \"nothing\""),
        ],
        results(&s)
    );
    let members = &s.reactor.contexts.get(c).unwrap().members;
    assert_eq!(1, members.len());
    assert_eq!(Some(wid(1)), members[0].window());
    assert_eq!(1, saved(&s).get(c).unwrap().members.len());
}

/// R23. A record goes only while the index still names the same app and
/// title. Here the record before the named one goes first, so the index
/// names another closed record: the command fails instead of removing it.
#[test]
fn remove_record_does_not_remove_a_record_that_shifted() {
    let mut s = Setup::new(3);
    let c = id_of(s.create("C", &[wid(1), wid(2)]));
    s.switch(ContextKey::Named(c));
    // Both member windows close, so both records are pending. Answer the
    // frame writes that each close causes before the next one.
    for idx in [1, 2] {
        s.apps.windows.remove(&wid(idx));
        s.reactor.handle_event(Event::WindowDestroyed(wid(idx)));
        s.apps.simulate_until_quiet(&mut s.reactor);
    }

    // The client read record 0 as Window1. After Window1 goes, the index
    // names the record of Window2, but the client's item still says
    // Window1.
    request(
        &mut s,
        1,
        ContextCommand::RemoveRecord {
            context: ContextRef::Id(c),
            record: record_ref(0, "TestApp1", "Window1"),
        },
    );
    request(
        &mut s,
        2,
        ContextCommand::RemoveRecord {
            context: ContextRef::Id(c),
            record: record_ref(0, "TestApp1", "Window1"),
        },
    );

    assert_eq!(
        vec![
            ran(1),
            failed(
                2,
                "The member record changed since it was listed; list the contexts again"
            ),
        ],
        results(&s)
    );
    let members = &s.reactor.contexts.get(c).unwrap().members;
    assert_eq!(1, members.len());
    assert_eq!("Window2", members[0].title);
}

/// R36. An edit that names any tab of a native group applies to the whole
/// group: the group joins or leaves the context together.
#[test]
fn an_edit_acts_on_a_whole_native_tab_group() {
    let mut s = Setup::new(2);
    // Window 3 joins window 2's native tab group while Everything shows, so
    // both keep the group's tile.
    focus_quietly(&mut s, wid(2));
    let tab = WindowInfo {
        frame: s.frame(wid(2)),
        ..make_window(3)
    };
    super::native_tabs::observe(&mut s, 1, wid(2), &[Some(wid(2))]);
    super::native_tabs::observe(&mut s, 1, wid(3), &[Some(wid(2)), Some(wid(3))]);
    open_window(&mut s, wid(3), tab, &[wid(1), wid(2)]);
    assert_eq!(vec![wid(2), wid(3)], s.reactor.tabs_of(wid(2)));
    let c = ContextKey::Named(id_of(s.create("C", &[wid(1)])));

    // Naming the tab, not the group's main tab, adds the whole group.
    request(
        &mut s,
        1,
        ContextCommand::EditContextMembers {
            context: ContextRef::Id(id_of(c)),
            add: vec![wid(3)],
            remove: vec![],
            remove_records: vec![],
        },
    );
    assert!(s.reactor.contexts.is_member(c, wid(2)));
    assert!(s.reactor.contexts.is_member(c, wid(3)));

    // Naming the main tab removes the whole group.
    request(
        &mut s,
        2,
        ContextCommand::EditContextMembers {
            context: ContextRef::Id(id_of(c)),
            add: vec![],
            remove: vec![wid(2)],
            remove_records: vec![],
        },
    );
    assert!(!s.reactor.contexts.is_member(c, wid(2)));
    assert!(!s.reactor.contexts.is_member(c, wid(3)));

    // And back.
    request(
        &mut s,
        3,
        ContextCommand::EditContextMembers {
            context: ContextRef::Id(id_of(c)),
            add: vec![wid(3)],
            remove: vec![],
            remove_records: vec![],
        },
    );
    assert!(s.reactor.contexts.is_member(c, wid(2)));
    assert!(s.reactor.contexts.is_member(c, wid(3)));
}

/// The new commands survive the RON round trip that recordings use, each
/// as itself.
#[test]
fn the_management_commands_survive_a_ron_round_trip() {
    let id = context_id(7);
    let commands = [
        ContextCommand::RenameContext {
            context: ContextRef::Number(1),
            name: "Client work".into(),
        },
        ContextCommand::SetContextNumber {
            context: ContextRef::Name("Comms".into()),
            number: 2,
        },
        ContextCommand::DeleteContext(ContextRef::Number(3)),
        ContextCommand::EditContextMembers {
            context: ContextRef::Id(id),
            add: vec![wid(1)],
            remove: vec![wid(2)],
            remove_records: vec![RecordRef {
                record: 0,
                app: "Mail".into(),
                title: "Inbox".into(),
            }],
        },
        ContextCommand::RemoveRecord {
            context: ContextRef::Number(1),
            record: RecordRef {
                record: 0,
                app: "Mail".into(),
                title: "Inbox".into(),
            },
        },
    ];
    for command in commands {
        let event = Event::Command(Command::Context(command.clone()));
        let ron = ron::ser::to_string(&event).unwrap();
        let Event::Command(Command::Context(back)) = ron::de::from_str(&ron).unwrap() else {
            panic!("{ron}")
        };
        assert_eq!(command, back, "{ron}");
    }
}

// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Publishing the contexts for the threads that read them (I1).

use std::sync::Arc;

use super::Reactor;
use crate::actor::contexts_snapshot::{
    CommandResult, ContextsSnapshot, MAX_COMMAND_RESULTS, RequestId, ScreenContext,
};
use crate::model::contexts::Scope;

impl Reactor {
    /// The contexts as the command line and the switcher see them, with the
    /// results of the last commands from the command line.
    fn contexts_snapshot(&self) -> ContextsSnapshot {
        ContextsSnapshot {
            results: self.command_results.iter().cloned().collect(),
            ..self.contexts_state()
        }
    }

    /// The contexts, with the facts that only the reactor knows: what each
    /// visible screen shows, and how many windows are unsorted.
    fn contexts_state(&self) -> ContextsSnapshot {
        if !self.contexts_enabled() {
            return ContextsSnapshot::off();
        }
        let screens = self
            .screens
            .iter()
            .filter_map(|screen| {
                Some(ScreenContext {
                    id: screen.id.get(),
                    shows: self.shown_context(screen.space?),
                })
            })
            .collect();
        let mut snapshot = ContextsSnapshot::new(
            &self.contexts,
            self.scope(),
            screens,
            self.unsorted_windows().len(),
        );
        if self.scope() == Scope::PerScreen {
            let focused = self.screens.get(self.focused_screen_index());
            snapshot.focused_screen = focused.map(|screen| screen.id.get());
            snapshot.active = self.screen_key(self.focused_screen_index());
        }
        snapshot
    }

    /// Keeps the result of a command from the command line for the next
    /// snapshots, and drops the oldest results beyond
    /// [`MAX_COMMAND_RESULTS`].
    pub(super) fn record_command_result(&mut self, request: RequestId, error: Option<String>) {
        self.command_results.push_back(CommandResult { request, error });
        while self.command_results.len() > MAX_COMMAND_RESULTS {
            self.command_results.pop_front();
        }
    }

    /// Publishes the snapshot of the contexts when it differs from the one
    /// published last.
    pub(super) fn publish_contexts_snapshot(&mut self) {
        let snapshot = self.contexts_snapshot();
        if self.published_contexts.as_deref() == Some(&snapshot) {
            return;
        }
        let snapshot = Arc::new(snapshot);
        self.published_contexts = Some(snapshot.clone());
        (self.publish_contexts)(snapshot);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    use objc2_core_foundation::CGPoint;
    use pretty_assertions::assert_eq;
    use test_log::test;

    use super::super::create_context::tests::*;
    use super::super::testing::*;
    use super::super::{
        Command, ContextCommand, ContextRef, Event, Reactor, ReactorCommand, Requested,
    };
    use crate::actor::app::{Quiet, Request, WindowId};
    use crate::actor::contexts_snapshot::{
        CONTEXTS_OFF, CommandResult, ContextSummary, ContextsSnapshot, MAX_COMMAND_RESULTS,
        MemberSummary, RequestId, ScreenContext, app_name,
    };
    use crate::actor::layout::{LayoutCommand, LayoutEvent, LayoutManager};
    use crate::actor::parked_journal::{FailingWrites, ParkedJournal};
    use crate::actor::server::{ContextRequest, Response, answer_context_request};
    use crate::model::Direction;
    use crate::model::contexts::{ContextId, ContextKey, Scope, WindowDesc};
    use crate::sys::app::WindowInfo;
    use crate::sys::screen::{ScreenId, SpaceId};
    use crate::sys::window_server::{WindowServerId, WindowServerInfo, WindowsOnScreen};

    type Published = Arc<Mutex<Vec<Arc<ContextsSnapshot>>>>;

    /// Keeps every snapshot the reactor publishes from now on.
    fn capture(reactor: &mut Reactor) -> Published {
        let published = Published::default();
        let sink = published.clone();
        reactor.publish_contexts = Box::new(move |snapshot| sink.lock().unwrap().push(snapshot));
        reactor.published_contexts = None;
        published
    }

    fn count(published: &Published) -> usize {
        published.lock().unwrap().len()
    }

    fn last(published: &Published) -> ContextsSnapshot {
        (**published.lock().unwrap().last().unwrap()).clone()
    }

    fn shows(key: ContextKey) -> Vec<ScreenContext> {
        vec![ScreenContext { id: 1, shows: key }]
    }

    /// Per-screen snapshots use stable display ids and report the focused
    /// display's context as active for the menu and command line.
    #[test]
    fn a_per_screen_snapshot_follows_the_focused_display() {
        let mut s = Setup::on(
            vec![screen(), rect(1200., 0., 1200., 1000.)],
            vec![Some(space()), Some(SpaceId::new(2))],
        );
        let mut per_screen = crate::config::Config::default();
        per_screen.settings.default_disable = false;
        per_screen.settings.experimental.contexts.enable = true;
        per_screen.settings.experimental.contexts.scope = Scope::PerScreen;
        s.reactor.handle_event(Event::ConfigChanged(Arc::new(per_screen)));
        s.reactor.screens[0].id = ScreenId::new(17);
        s.reactor.screens[1].id = ScreenId::new(42);
        let c = s.reactor.contexts.create("C").unwrap();
        s.reactor
            .contexts
            .switch_to_on(ScreenId::new(42), ContextKey::Named(c))
            .unwrap();
        s.reactor.active_screen_idx = Some(1);

        let snapshot = s.reactor.contexts_state();

        assert_eq!(Some(42), snapshot.focused_screen);
        assert_eq!(ContextKey::Named(c), snapshot.active);
        assert_eq!(Some(ContextKey::Named(c)), snapshot.shown());
        assert_eq!(
            vec![17, 42],
            snapshot.screens.iter().map(|screen| screen.id).collect::<Vec<_>>()
        );
        s.reactor.active_screen_idx = Some(0);
        let snapshot = s.reactor.contexts_state();
        assert_eq!(ContextKey::Everything, snapshot.active);
        assert_eq!(Some(ContextKey::Everything), snapshot.shown());
    }

    /// A window server snapshot that lists app 1's windows at their frames.
    fn listed(s: &Setup, idxs: &[u32]) -> WindowsOnScreen {
        WindowsOnScreen::new(
            idxs.iter()
                .map(|&idx| WindowServerInfo {
                    id: WindowServerId::new(idx),
                    pid: 1,
                    layer: 0,
                    frame: s.apps.windows[&wid(idx)].frame,
                })
                .collect(),
        )
    }

    /// I1. The reactor publishes a snapshot after each event that changes
    /// the contexts or the windows they count, and only then. The first
    /// snapshot is published even when contexts are off.
    #[test]
    fn a_snapshot_is_published_after_each_kind_of_change() {
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let published = capture(&mut reactor);
        let mut s = Setup {
            reactor,
            apps: Apps::new(),
            dir: tempfile::TempDir::new().unwrap(),
        };

        s.reactor.handle_event(screens(vec![screen()], vec![Some(space())]));
        assert_eq!(1, count(&published));
        assert_eq!(ContextsSnapshot::off(), last(&published));

        s.reactor.handle_event(Event::ConfigChanged(config(true)));
        let on = last(&published);
        assert!(on.enabled);
        assert_eq!(ContextKey::Everything, on.active);
        assert_eq!(shows(ContextKey::Everything), on.screens);
        assert!(on.contexts.is_empty());
        assert_eq!(0, on.unsorted.windows);

        // Windows open. Before the first context exists, no window is
        // unsorted, so nothing the snapshot holds changes.
        let before = count(&published);
        s.reactor.handle_events(s.apps.make_app(1, make_windows(3)));
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(before, count(&published));
        assert_eq!(on, last(&published));
        assert!(!on.unsorted.listed);

        // Nothing changes.
        let before = count(&published);
        s.reactor.handle_event(Event::MouseUp);
        s.reactor.handle_event(Event::StartupComplete);
        assert_eq!(before, count(&published));

        // A context is created.
        s.create("Work");
        let work = s.id("Work");
        let created = last(&published);
        assert_eq!(vec![summary(&s, "Work", 3)], created.contexts);
        assert_eq!(ContextKey::Named(work), created.active);
        assert_eq!(shows(ContextKey::Named(work)), created.screens);
        assert_eq!(0, created.unsorted.windows);

        // A switch.
        s.run(ContextCommand::ShowEverything);
        let switched = last(&published);
        assert_eq!(ContextKey::Everything, switched.active);
        assert_eq!(2, switched.everything.last_used);

        // A window opens and closes.
        let other = WindowId::new(2, 1);
        let other_window = WindowInfo {
            sys_id: Some(WindowServerId::new(20)),
            ..make_window(4)
        };
        s.reactor.handle_events(s.apps.make_app(2, vec![other_window]));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(1, last(&published).unsorted.windows);
        assert!(last(&published).unsorted.listed);
        s.apps.windows.remove(&other);
        s.reactor.handle_event(Event::WindowDestroyed(other));
        assert_eq!(0, last(&published).unsorted.windows);
        assert!(!last(&published).unsorted.listed);

        // The Space is about to be turned off, so it shows every window
        // until the next space change.
        s.run(ContextCommand::SwitchContext(ContextRef::Id(work)));
        s.reactor.handle_event(Event::ShowEverythingOn(vec![space()]));
        let turning_off = last(&published);
        assert_eq!(ContextKey::Named(work), turning_off.active);
        assert_eq!(shows(ContextKey::Everything), turning_off.screens);
        let all = listed(&s, &[1, 2, 3]);
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space())], all));
        assert_eq!(shows(ContextKey::Named(work)), last(&published).screens);

        // The context is deleted. Unsorted is active, and without a context
        // no window counts as unsorted.
        s.run(ContextCommand::DeleteContext(ContextRef::Id(work)));
        let deleted = last(&published);
        assert!(deleted.contexts.is_empty());
        assert_eq!(ContextKey::Unsorted, deleted.active);
        assert_eq!(0, deleted.unsorted.windows);
        assert!(!deleted.unsorted.listed);

        // Contexts are turned off.
        s.reactor.handle_event(Event::ConfigChanged(config(false)));
        assert_eq!(ContextsSnapshot::off(), last(&published));
    }

    /// The Unsorted count follows the windows on the visible Spaces: a
    /// window that leaves them, for example by being minimized, stops
    /// counting.
    #[test]
    fn unsorted_counts_the_windows_on_the_visible_spaces() {
        let mut s = Setup::new(3);
        s.reactor.contexts.create("Empty").unwrap();
        let published = capture(&mut s.reactor);
        s.reactor.handle_event(Event::StartupComplete);
        assert_eq!(3, last(&published).unsorted.windows);

        let on_screen = listed(&s, &[1, 2]);
        s.reactor.handle_event(Event::WindowsOnScreenUpdated { pid: None, on_screen });

        assert_eq!(2, last(&published).unsorted.windows);
    }

    /// App 1's window as the contexts model describes it.
    fn desc(idx: u32) -> WindowDesc {
        WindowDesc {
            wid: wid(idx),
            bundle_id: Some("com.testapp1".into()),
            app_name: Some("TestApp1".into()),
            title: format!("Window{idx}"),
            window_server_id: Some(WindowServerId::new(idx)),
        }
    }

    /// Adds app 1's windows to the context by the model, as a membership
    /// command will.
    fn add(s: &mut Setup, id: ContextId, idxs: &[u32]) {
        for &idx in idxs {
            s.reactor.contexts.add_window(id, &desc(idx)).unwrap();
        }
    }

    fn summary(s: &Setup, name: &str, windows: usize) -> ContextSummary {
        let context = s.reactor.contexts.by_name(name).unwrap();
        ContextSummary {
            id: context.id,
            name: name.into(),
            number: context.number,
            last_used: context.last_used,
            apps: if windows > 0 {
                vec!["TestApp1".into()]
            } else {
                vec![]
            },
            windows,
            // The counts are what these tests pin; the records are the
            // model's, as `ContextSummary::new` projects them.
            members: context
                .members
                .iter()
                .enumerate()
                .map(|(record, member)| MemberSummary {
                    record,
                    app: app_name(member),
                    title: member.title.clone(),
                    window: member.window(),
                })
                .collect(),
        }
    }

    /// R1, R29. A window in two contexts counts in both. An unsorted window
    /// counts while it is parked, because it is still on a visible Space.
    #[test]
    fn a_window_counts_in_each_of_its_contexts_and_a_parked_unsorted_one_counts() {
        let mut s = Setup::new(4);
        let c = s.reactor.contexts.create("C").unwrap();
        let d = s.reactor.contexts.create("D").unwrap();
        add(&mut s, c, &[1, 2]);
        add(&mut s, d, &[2, 3]);
        let published = capture(&mut s.reactor);

        s.run(ContextCommand::SwitchContext(ContextRef::Id(c)));

        assert_eq!(vec![wid(3), wid(4)], s.parked());
        let switched = last(&published);
        assert_eq!(vec![summary(&s, "C", 2), summary(&s, "D", 2)], switched.contexts);
        assert_eq!(1, switched.unsorted.windows);
        assert_eq!(ContextKey::Named(c), switched.active);
        assert_eq!(shows(ContextKey::Named(c)), switched.screens);
    }

    /// R3. A pinned window shows under Unsorted but doesn't count as an
    /// unsorted window.
    #[test]
    fn a_pinned_window_does_not_count_as_unsorted() {
        let mut s = Setup::new(3);
        let c = s.reactor.contexts.create("C").unwrap();
        add(&mut s, c, &[1]);
        let published = capture(&mut s.reactor);
        s.reactor.handle_event(Event::StartupComplete);
        assert_eq!(2, last(&published).unsorted.windows);

        s.reactor.contexts.pin(&desc(3));
        s.run(ContextCommand::SwitchContext(ContextRef::Name(
            "Unsorted".into(),
        )));

        assert_eq!(ContextKey::Unsorted, last(&published).active);
        assert_eq!(vec![wid(1)], s.parked());
        assert_eq!(1, last(&published).unsorted.windows);
        assert_eq!(vec![summary(&s, "C", 2)], last(&published).contexts);
    }

    /// R28, R29. Before the first context exists, no window is unsorted:
    /// the snapshot counts none and doesn't list Unsorted, and its name
    /// names nothing, so Everything stays active.
    #[test]
    fn unsorted_is_neither_counted_nor_named_before_the_first_context_exists() {
        let mut s = Setup::new(2);
        let published = capture(&mut s.reactor);
        s.reactor.handle_event(Event::StartupComplete);
        let none = last(&published);
        assert_eq!((false, 0), (none.unsorted.listed, none.unsorted.windows));

        request(
            &mut s,
            1,
            ContextCommand::SwitchContext(ContextRef::Name("unsorted".into())),
        );

        assert_eq!(
            Response::Error("No context matches \"unsorted\"".into()),
            result_of(&s, 1)
        );
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert!(s.parked().is_empty());
    }

    /// R29. While every window is in a context, Unsorted isn't listed, so
    /// a name that only Unsorted's name starts, and Unsorted's own name,
    /// name nothing. No switch goes to an empty Unsorted, which would park
    /// every window.
    #[test]
    fn a_name_that_only_an_unlisted_unsorted_matches_names_nothing() {
        let mut s = Setup::new(2);
        s.create("Work");
        let work = ContextKey::Named(s.id("Work"));
        assert!(!s.reactor.published_contexts.clone().unwrap().unsorted.listed);

        for (id, name) in [(1, "uns"), (2, "Unsorted")] {
            request(
                &mut s,
                id,
                ContextCommand::SwitchContext(ContextRef::Name(name.into())),
            );
            assert_eq!(
                Response::Error(format!("No context matches \"{name}\"")),
                result_of(&s, id)
            );
        }

        assert_eq!(work, s.reactor.contexts.active());
        assert!(s.parked().is_empty());
    }

    /// I1. A member window that closes no longer counts as open, in each of
    /// its contexts, and its app drops out of a context without another open
    /// window of it.
    #[test]
    fn closing_a_member_window_publishes_the_lower_counts() {
        let mut s = Setup::new(3);
        let c = s.reactor.contexts.create("C").unwrap();
        let d = s.reactor.contexts.create("D").unwrap();
        add(&mut s, c, &[1, 2]);
        add(&mut s, d, &[2]);
        let published = capture(&mut s.reactor);
        s.reactor.handle_event(Event::StartupComplete);
        assert_eq!(
            vec![summary(&s, "C", 2), summary(&s, "D", 1)],
            last(&published).contexts
        );

        s.apps.windows.remove(&wid(2));
        s.reactor.handle_event(Event::WindowDestroyed(wid(2)));

        assert_eq!(
            vec![summary(&s, "C", 1), summary(&s, "D", 0)],
            last(&published).contexts
        );
        assert_eq!(1, last(&published).unsorted.windows);
    }

    /// I1. Each screen that shows a Space gets an entry, numbered by its
    /// place in the list of screens from 1. A screen that shows no Space
    /// gets none. A display that goes away takes its entry with it.
    #[test]
    fn each_screen_with_a_space_gets_an_entry_numbered_by_its_place() {
        let left = screen();
        let middle = rect(1200., 0., 1200., 1000.);
        let right = rect(2400., 0., 1200., 1000.);
        let mut s = Setup::on(
            vec![left, middle, right],
            vec![Some(space()), None, Some(SpaceId::new(2))],
        );
        let published = capture(&mut s.reactor);
        s.reactor.handle_event(Event::StartupComplete);
        let everywhere = |key| {
            vec![
                ScreenContext { id: 1, shows: key },
                ScreenContext { id: 3, shows: key },
            ]
        };
        assert_eq!(everywhere(ContextKey::Everything), last(&published).screens);

        s.create("Work");

        let work = ContextKey::Named(s.id("Work"));
        assert_eq!(everywhere(work), last(&published).screens);
        s.reactor.handle_event(screens(vec![left], vec![Some(space())]));
        assert_eq!(shows(work), last(&published).screens);
    }

    /// I1. Events and commands that change nothing the snapshot holds
    /// publish nothing: a layout command, a switch whose journal write
    /// fails, a switch to a context that doesn't exist or that no name
    /// matches, a refused new context, and a previous context that doesn't
    /// exist. Each change that follows publishes once.
    #[test]
    fn events_that_change_nothing_in_the_snapshot_publish_nothing() {
        let mut s = Setup::new(2);
        let c = s.reactor.contexts.create("C").unwrap();
        add(&mut s, c, &[1]);
        let published = capture(&mut s.reactor);
        s.reactor.handle_event(Event::StartupComplete);
        assert_eq!(1, count(&published));

        let frames = s.frames(&[wid(1), wid(2)]);
        s.reactor.send_layout_event(LayoutEvent::WindowFocused(vec![space()], wid(1)));
        s.reactor.handle_event(Event::Command(Command::Layout(LayoutCommand::MoveNode(
            Direction::Right,
        ))));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_ne!(frames, s.frames(&[wid(1), wid(2)]));
        let failing = FailingWrites::start(s.dir.path());
        s.run(ContextCommand::SwitchContext(ContextRef::Id(c)));
        drop(failing);
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        let deleted = s.reactor.contexts.create("Gone").unwrap();
        s.reactor.contexts.delete(deleted).unwrap();
        s.run(ContextCommand::SwitchContext(ContextRef::Id(deleted)));
        s.run(ContextCommand::SwitchContext(ContextRef::Number(2)));
        s.run(ContextCommand::SwitchContext(ContextRef::Name("zzz".into())));
        for name in ["c", "Everything", " "] {
            s.create(name);
        }
        s.run(ContextCommand::PreviousContext);
        assert_eq!(1, count(&published));

        s.run(ContextCommand::SwitchContext(ContextRef::Id(c)));
        assert_eq!(2, count(&published));
        assert_eq!(ContextKey::Named(c), last(&published).active);
        s.run(ContextCommand::PreviousContext);
        assert_eq!(3, count(&published));
        assert_eq!(ContextKey::Everything, last(&published).active);
    }

    /// Pointer moves, drags, and scrolls come many times a second and change
    /// nothing that the snapshot holds, so the reactor builds no snapshot
    /// for them. The next other event publishes what changed meanwhile.
    #[test]
    fn pointer_and_scroll_events_build_no_snapshot() {
        let mut s = Setup::new(2);
        let published = capture(&mut s.reactor);
        s.reactor.contexts.create("Direct").unwrap();

        s.reactor
            .handle_event(Event::MouseMovedOverWindow(WindowServerId::new(1), None));
        s.reactor.handle_event(Event::LeftMouseDragged(CGPoint::new(10., 10.)));
        s.reactor.handle_event(Event::ScrollWheel {
            delta_x: 0.,
            delta_y: 1.,
            alt_held: true,
        });
        assert_eq!(0, count(&published));

        s.reactor.handle_event(Event::MouseUp);
        assert_eq!(1, count(&published));
        assert_eq!("Direct", last(&published).contexts[0].name);
    }

    /// R16, R19. Switching to the active context again is a use of it, so
    /// the snapshot with its new use number is published.
    #[test]
    fn switching_to_the_active_context_again_publishes_its_new_use() {
        let mut s = Setup::new(2);
        s.create("Work");
        let work = s.id("Work");
        let published = capture(&mut s.reactor);
        s.reactor.handle_event(Event::StartupComplete);
        let used = last(&published).contexts[0].last_used;

        s.run(ContextCommand::SwitchContext(ContextRef::Id(work)));

        assert_eq!(2, count(&published));
        assert_eq!(used + 1, last(&published).contexts[0].last_used);
    }

    /// R28, I1. With contexts turned off, the snapshot is the one of contexts
    /// that are off, and nothing else is published until they are on again.
    /// The server then refuses every request. Turned on again, the contexts
    /// are published as they were.
    #[test]
    fn with_contexts_off_only_the_off_snapshot_is_published() {
        let mut s = Setup::new(2);
        s.create("Work");
        let work = s.id("Work");
        let published = capture(&mut s.reactor);
        s.reactor.handle_event(Event::StartupComplete);
        let on = last(&published);

        s.reactor.handle_event(Event::ConfigChanged(config(false)));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(2, count(&published));
        let off = last(&published);
        assert_eq!(ContextsSnapshot::off(), off);
        s.run(ContextCommand::CreateContext("Other".into()));
        s.run(ContextCommand::SwitchContext(ContextRef::Id(work)));
        s.run(ContextCommand::ShowEverything);
        s.reactor.handle_event(Event::MouseUp);
        assert_eq!(2, count(&published));
        for request in [
            ContextRequest::List,
            ContextRequest::Current,
            ContextRequest::Run(RequestId(1), ContextCommand::ShowEverything),
            ContextRequest::Run(RequestId(2), ContextCommand::CreateContext("Other".into())),
        ] {
            let (reply, sent) = answer_context_request(request, Some(&off));
            assert_eq!(None, sent);
            assert_eq!(Response::Error(CONTEXTS_OFF.into()), reply);
        }

        s.reactor.handle_event(Event::ConfigChanged(config(true)));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert_eq!(3, count(&published));
        assert_eq!(on, last(&published));
    }

    /// R32. While a quit waits for parked windows to come back, each screen
    /// shows Everything, and the active context stays.
    #[test]
    fn a_quit_that_waits_publishes_everything_on_each_screen() {
        let mut s = Setup::new(2);
        let c = s.reactor.contexts.create("C").unwrap();
        add(&mut s, c, &[1]);
        s.run(ContextCommand::SwitchContext(ContextRef::Id(c)));
        assert_eq!(vec![wid(2)], s.parked());
        let published = capture(&mut s.reactor);

        s.reactor
            .handle_event(Event::Command(Command::Reactor(ReactorCommand::SaveAndExit)));

        let quitting = last(&published);
        assert_eq!(ContextKey::Named(c), quitting.active);
        assert_eq!(shows(ContextKey::Everything), quitting.screens);
    }

    /// Sends a context command from the command line with request id
    /// `request`, and lets the apps answer.
    fn request(s: &mut Setup, request: u64, command: ContextCommand) {
        s.reactor
            .handle_event(Event::ContextCommandRequested(RequestId(request), command));
        s.apps.simulate_until_quiet(&mut s.reactor);
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

    fn start_park_command(
        s: &mut Setup,
        request: u64,
    ) -> (objc2_core_foundation::CGRect, super::super::TransactionId) {
        let empty = s.reactor.contexts.create("Empty").unwrap();
        s.reactor.handle_event(Event::ContextCommandRequested(
            RequestId(request),
            ContextCommand::SwitchContext(ContextRef::Id(empty)),
        ));
        assert_eq!(Response::Pending, result_of(s, request));
        s.apps
            .requests()
            .into_iter()
            .find_map(|request| match request {
                Request::SetWindowFrame(window, frame, txid) if window == wid(1) => {
                    Some((frame, txid))
                }
                _ => None,
            })
            .expect("parking write")
    }

    /// A requested echo reports AX's observed frame even when the app refused
    /// every retry. The command fails and the saved restore frame survives.
    #[test]
    fn a_refused_park_does_not_complete_the_switch_or_drop_the_journal() {
        let mut s = Setup::new(1);
        let before = s.apps.windows[&wid(1)].frame;
        let (target, txid) = start_park_command(&mut s, 1);
        assert_ne!(before, target);

        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            before,
            txid,
            Requested(true),
            None,
        ));

        assert!(matches!(result_of(&s, 1), Response::Error(_)));
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert!(s.reactor.parked.is_empty());
        let journal = ParkedJournal::open(s.dir.path().join("parked.json"), SystemTime::now());
        assert_eq!(before, journal.entries()[0].frame.into());
        assert_eq!(1, journal.entries().len(), "restore waits for its own echo");
        let restores: Vec<_> = s
            .apps
            .requests()
            .into_iter()
            .filter_map(|request| match request {
                Request::SetWindowFrame(window, frame, txid) if window == wid(1) => {
                    Some((frame, txid))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            1,
            restores.len(),
            "abort sends one restore, without another park"
        );
        assert_eq!(before, restores[0].0);
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            before,
            restores[0].1,
            Requested(true),
            None,
        ));
        assert!(
            ParkedJournal::open(s.dir.path().join("parked.json"), SystemTime::now())
                .entries()
                .is_empty()
        );
    }

    #[test]
    fn textedit_clamped_park_is_rejected_and_keeps_the_original_frame() {
        let visible = rect(0., 33., 1512., 949.);
        let before = rect(305., 367., 586., 488.);
        let mut s = Setup::on(vec![visible], vec![Some(space())]);
        let mut window = make_window(1);
        window.frame = before;
        s.reactor.handle_events(s.apps.make_app(1, vec![window]));
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        s.reactor.windows.get_mut(&wid(1)).unwrap().frame_monotonic = before;
        s.apps.windows.get_mut(&wid(1)).unwrap().frame = before;
        let (target, txid) = start_park_command(&mut s, 2);
        assert_eq!(rect(1511., 981., 586., 488.), target);

        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            rect(1511., 950., 586., 488.),
            txid,
            Requested(true),
            None,
        ));

        assert!(matches!(result_of(&s, 2), Response::Error(_)));
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        let journal = ParkedJournal::open(s.dir.path().join("parked.json"), SystemTime::now());
        assert_eq!(before, journal.entries()[0].frame.into());
        assert_eq!(1, journal.entries().len());
    }

    #[test]
    fn unrelated_command_result_does_not_wait_for_an_older_park() {
        let mut s = Setup::new(1);
        let (target, txid) = start_park_command(&mut s, 3);
        let empty = s.reactor.contexts.by_name("Empty").unwrap().id;
        s.reactor.handle_event(Event::ContextCommandRequested(
            RequestId(4),
            ContextCommand::RenameContext {
                context: ContextRef::Id(empty),
                name: "Renamed".into(),
            },
        ));
        assert_eq!(Response::Success, result_of(&s, 4));
        assert_eq!(Response::Pending, result_of(&s, 3));
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            target,
            txid,
            Requested(true),
            None,
        ));
        assert_eq!(Response::Success, result_of(&s, 3));
    }

    #[test]
    fn a_second_switch_waits_for_the_first_unconfirmed_park() {
        let mut s = Setup::new(1);
        let (target, txid) = start_park_command(&mut s, 7);
        let empty = s.reactor.contexts.by_name("Empty").unwrap().id;
        s.reactor.handle_event(Event::ContextCommandRequested(
            RequestId(8),
            ContextCommand::SwitchContext(ContextRef::Id(empty)),
        ));
        assert_eq!(Response::Pending, result_of(&s, 8));
        assert_eq!(Response::Pending, result_of(&s, 7));
        s.reactor.handle_event(Event::WindowFrameChanged(
            wid(1),
            target,
            txid,
            Requested(true),
            None,
        ));
        assert_eq!(Response::Success, result_of(&s, 7));
        assert_eq!(Response::Success, result_of(&s, 8));
    }

    #[test]
    fn missing_park_echo_and_app_exit_finish_the_pending_result() {
        let mut timed_out = Setup::new(1);
        start_park_command(&mut timed_out, 5);
        timed_out.reactor.guard_deadline_tick(Instant::now() + Duration::from_secs(3));
        timed_out.reactor.publish_contexts_snapshot();
        assert!(matches!(result_of(&timed_out, 5), Response::Error(_)));
        assert_eq!(ContextKey::Everything, timed_out.reactor.contexts.active());

        let mut ended = Setup::new(1);
        start_park_command(&mut ended, 6);
        ended.reactor.handle_event(Event::ApplicationThreadTerminated(1));
        assert!(matches!(result_of(&ended, 6), Response::Error(_)));
    }

    /// A command from the command line publishes its result under its
    /// request id: nothing when it ran, and the reason when it did nothing.
    /// The server gives the command line the result from the snapshot.
    #[test]
    fn a_command_from_the_command_line_publishes_its_result() {
        let mut s = Setup::new(2);
        s.create("Work");
        let published = capture(&mut s.reactor);
        let switch = |name: &str| ContextCommand::SwitchContext(ContextRef::Name(name.into()));

        request(&mut s, 1, ContextCommand::ShowEverything);
        request(&mut s, 2, switch("wrk"));
        request(&mut s, 3, switch("nothing"));
        request(&mut s, 4, ContextCommand::CreateContext("work".into()));
        request(&mut s, 5, ContextCommand::PreviousContext);

        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert_eq!(
            vec![
                ran(1),
                ran(2),
                failed(3, "No context matches \"nothing\""),
                failed(4, "A context named \"Work\" already exists"),
                ran(5),
            ],
            last(&published).results
        );
        assert_eq!(5, count(&published));
        assert_eq!(Response::Success, result_of(&s, 2));
        assert_eq!(
            Response::Error("No context matches \"nothing\"".into()),
            result_of(&s, 3)
        );
        assert_eq!(Response::Pending, result_of(&s, 6));
    }

    /// R28. With contexts off, a command from the command line does nothing
    /// and publishes that contexts are off, in the snapshot of contexts
    /// that are off. So a command that the server took just before contexts
    /// were turned off gets that answer.
    #[test]
    fn a_command_while_contexts_are_off_publishes_that_they_are_off() {
        let mut s = Setup::new(2);
        s.create("Work");
        s.run(ContextCommand::ShowEverything);
        s.reactor.handle_event(Event::ConfigChanged(config(false)));
        s.apps.simulate_until_quiet(&mut s.reactor);
        let published = capture(&mut s.reactor);

        request(
            &mut s,
            1,
            ContextCommand::SwitchContext(ContextRef::Name("Work".into())),
        );

        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        assert!(s.parked().is_empty());
        assert_eq!(
            ContextsSnapshot {
                results: vec![failed(1, CONTEXTS_OFF)],
                ..ContextsSnapshot::off()
            },
            last(&published)
        );
        assert_eq!(Response::Error(CONTEXTS_OFF.into()), result_of(&s, 1));
    }

    /// The snapshot keeps the newest 32 results. The command line gets no
    /// answer for an older one, and so stops asking after about a second.
    #[test]
    fn the_snapshot_keeps_the_newest_results() {
        let mut s = Setup::new(1);
        s.create("Work");

        for id in 1..=40 {
            request(&mut s, id, ContextCommand::ShowEverything);
        }

        let results = s.reactor.published_contexts.clone().unwrap().results.clone();
        assert_eq!((9..=40).map(ran).collect::<Vec<_>>(), results);
        assert_eq!(MAX_COMMAND_RESULTS, results.len());
        assert_eq!(Response::Pending, result_of(&s, 8));
        assert_eq!(Response::Success, result_of(&s, 9));
    }

    /// `sugarglider context add` adds the focused window to the context
    /// that its query names, and publishes the result: the reason when no
    /// window has focus, when the query names no context, or when it names
    /// Everything or Unsorted, which take no window.
    #[test]
    fn an_add_from_the_command_line_publishes_its_result() {
        let mut s = Setup::new(2);
        let work = s.reactor.contexts.create("Work").unwrap();
        let add = |name: &str| ContextCommand::AddWindowToContext(ContextRef::Name(name.into()));
        request(&mut s, 1, add("Work"));
        s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
        s.reactor.handle_event(Event::ApplicationActivated(1, Quiet::Yes));
        s.reactor
            .handle_event(Event::ApplicationMainWindowChanged(1, Some(wid(2)), Quiet::Yes));

        request(&mut s, 2, add("wo"));
        request(&mut s, 3, add("Everything"));
        request(&mut s, 4, add("nothing"));

        assert_eq!(
            vec![
                failed(1, "No window has focus"),
                ran(2),
                failed(3, "Only a named context can take a window"),
                failed(4, "No context matches \"nothing\""),
            ],
            s.reactor.published_contexts.clone().unwrap().results
        );
        assert!(s.reactor.contexts.is_member(ContextKey::Named(work), wid(2)));
        assert!(!s.reactor.contexts.is_member(ContextKey::Named(work), wid(1)));
    }

    /// A command from a key binding or the menu, which has no request id,
    /// publishes no result.
    #[test]
    fn a_command_without_a_request_id_publishes_no_result() {
        let mut s = Setup::new(1);
        s.create("Work");
        s.run(ContextCommand::ShowEverything);
        s.run(ContextCommand::SwitchContext(ContextRef::Name("nothing".into())));

        assert!(s.reactor.published_contexts.clone().unwrap().results.is_empty());
    }
}

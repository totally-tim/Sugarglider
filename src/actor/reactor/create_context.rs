// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Creating a context from the windows on screen.

use tracing::info;

use super::Reactor;
use crate::actor::app::{WindowId, pid_t};
use crate::model::contexts::{ContextKey, WindowDesc};

impl Reactor {
    /// Creates a context whose members are the windows that show on the
    /// visible Spaces, and switches to it. Its layout starts as the layout
    /// the Spaces show (L3). Pinned windows get no record, because they are
    /// members of every context already (R3). The caller checks that
    /// contexts are on and that Sugarglider isn't quitting.
    pub(super) fn create_context(&mut self, name: &str) -> Result<(), String> {
        let id = self.contexts.create(name).map_err(|err| err.to_string())?;
        let members: Vec<WindowDesc> = self
            .windows_on_visible_spaces(|wid| {
                !self.parked.contains_key(&wid) && !self.contexts.is_pinned(wid)
            })
            .into_iter()
            .filter_map(|wid| self.window_desc(wid))
            .collect();
        for member in &members {
            self.contexts.add_window(id, member).expect("the context was just created");
        }
        info!(name, members = members.len(), "Created a context");
        self.save_contexts();
        self.switch_context(ContextKey::Named(id))
    }

    /// Creates a context whose members are exactly `windows`, and switches to
    /// it. Each window is resolved to its native tab group (R36), and a
    /// pinned window is left out: it is a member of every context already
    /// (R3). The caller checks that contexts are on and that Sugarglider
    /// isn't quitting.
    pub(super) fn create_context_from_windows(
        &mut self,
        name: &str,
        windows: &[WindowId],
    ) -> Result<(), String> {
        let id = self.contexts.create(name).map_err(|err| err.to_string())?;
        let members = self.group_windows(windows);
        for &wid in &members {
            if let Some(desc) = self.window_desc(wid) {
                self.contexts.add_window(id, &desc).expect("the context was just created");
            }
        }
        info!(name, members = members.len(), "Created a context");
        self.save_contexts();
        self.switch_context(ContextKey::Named(id))
    }

    /// The windows on the visible Spaces for which `keep` returns true, in id
    /// order. These are the windows of running apps that the window server
    /// lists as visible, on a screen that shows a Space, or on no screen
    /// while one does, as a switch finds them. Sugarglider's own windows and
    /// windows the layout doesn't track are left out. `keep` runs before the
    /// checks that copy the window's details.
    pub(super) fn windows_on_visible_spaces(
        &self,
        keep: impl Fn(WindowId) -> bool,
    ) -> Vec<WindowId> {
        let own_pid = std::process::id() as pid_t;
        let any_space = self.screens.iter().any(|screen| screen.space.is_some());
        let mut wids: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(wid, window)| {
                wid.pid != own_pid
                    && self.apps.contains_key(&wid.pid)
                    && window
                        .window_server_id
                        .is_some_and(|wsid| self.visible_windows.contains(&wsid))
            })
            .map(|(wid, _)| *wid)
            .filter(|&wid| keep(wid))
            .filter(|&wid| {
                let Some(info) = self.layout_window_info(wid) else {
                    return false;
                };
                let on_space = match self.best_screen_idx_for_window(&info.frame) {
                    Some(screen) => self.screens[screen].space.is_some(),
                    None => any_space,
                };
                on_space && !self.layout.is_untracked(&info)
            })
            .collect();
        wids.sort();
        wids
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::Arc;
    use std::time::SystemTime;

    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use test_log::test;

    use super::super::testing::*;
    use super::super::{Command, ContextCommand, ContextRef, Event, Reactor, ReactorCommand};
    use crate::actor::app::WindowId;
    use crate::actor::contexts_snapshot::{ContextsSnapshot, RequestId};
    use crate::actor::contexts_store::{ContextsStore, Loaded};
    use crate::actor::layout::{LayoutCommand, LayoutEvent, LayoutManager};
    use crate::actor::parked_journal::ParkedJournal;
    use crate::actor::server::{ContextRequest, Response, answer_context_request};
    use crate::config::Config;
    use crate::model::contexts::{ContextId, ContextKey, Contexts};
    use crate::sys::app::WindowInfo;
    use crate::sys::screen::{CoordinateConverter, SpaceId};
    use crate::sys::window_server::{WindowServerId, WindowServerInfo, WindowsOnScreen};

    pub fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    pub fn wid(idx: u32) -> WindowId {
        WindowId::new(1, idx)
    }

    pub fn space() -> SpaceId {
        SpaceId::new(1)
    }

    pub fn screen() -> CGRect {
        rect(0., 0., 1200., 1000.)
    }

    pub fn config(contexts: bool) -> Arc<Config> {
        let mut config = Config::default();
        config.settings.default_disable = false;
        config.settings.animate = false;
        config.settings.experimental.contexts.enable = contexts;
        Arc::new(config)
    }

    pub fn screens(frames: Vec<CGRect>, spaces: Vec<Option<SpaceId>>) -> Event {
        Event::ScreenParametersChanged {
            ids: vec![],
            bounds: frames.clone(),
            scale_factors: vec![1.0; frames.len()],
            frames,
            spaces,
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        }
    }

    pub fn context(command: ContextCommand) -> Event {
        Event::Command(Command::Context(command))
    }

    /// A reactor with contexts on, its journal and `contexts.json` in a
    /// temporary directory.
    pub struct Setup {
        pub reactor: Reactor,
        pub apps: Apps,
        pub dir: TempDir,
    }

    impl Setup {
        /// App 1's `windows` windows tiled side by side on one screen.
        pub fn new(windows: usize) -> Setup {
            let mut s = Setup::on(vec![screen()], vec![Some(space())]);
            s.reactor.handle_events(s.apps.make_app(1, make_windows(windows)));
            s.reactor.handle_event(Event::StartupComplete);
            s.apps.simulate_until_quiet(&mut s.reactor);
            s
        }

        /// A reactor with contexts on that no app has reached yet.
        pub fn on(frames: Vec<CGRect>, spaces: Vec<Option<SpaceId>>) -> Setup {
            let dir = TempDir::new().unwrap();
            let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
            reactor.journal =
                ParkedJournal::open(dir.path().join("parked.json"), SystemTime::now());
            reactor.open_contexts(
                ContextsStore::new(dir.path().join("contexts.json")),
                Some("boot".into()),
                SystemTime::now(),
            );
            reactor.handle_event(Event::ConfigChanged(config(true)));
            reactor.handle_event(screens(frames, spaces));
            Setup {
                reactor,
                apps: Apps::new(),
                dir,
            }
        }

        /// Sends a context command, and lets the apps answer.
        pub fn run(&mut self, command: ContextCommand) {
            self.reactor.handle_event(context(command));
            self.apps.simulate_until_quiet(&mut self.reactor);
        }

        pub fn create(&mut self, name: &str) {
            self.run(ContextCommand::CreateContext(name.into()));
        }

        pub fn id(&self, name: &str) -> ContextId {
            self.reactor.contexts.by_name(name).unwrap().id
        }

        /// The open member windows of the named context.
        pub fn members(&self, name: &str) -> Vec<WindowId> {
            let context = self.reactor.contexts.by_name(name).unwrap();
            context.members.iter().filter_map(|record| record.window()).collect()
        }

        pub fn frames(&self, wids: &[WindowId]) -> Vec<(WindowId, CGRect)> {
            wids.iter().map(|&wid| (wid, self.apps.windows[&wid].frame)).collect()
        }

        pub fn parked(&self) -> Vec<WindowId> {
            let mut parked: Vec<WindowId> = self.reactor.parked.keys().copied().collect();
            parked.sort();
            parked
        }

        pub fn saved(&self) -> Contexts {
            match ContextsStore::new(self.dir.path().join("contexts.json")).load(SystemTime::now())
            {
                Loaded::Read { contexts, .. } => contexts,
                other => panic!("{other:?}"),
            }
        }
    }

    /// The members are the windows that show on the visible Spaces.
    /// Sugarglider's own window, a panel the layout leaves alone, a
    /// minimized window, and a window on a screen whose Space is off stay
    /// out.
    #[test]
    fn a_new_context_holds_the_tracked_windows_on_the_visible_spaces() {
        let right = rect(1200., 0., 1200., 1000.);
        let mut s = Setup::on(vec![screen(), right], vec![Some(space()), None]);
        let own_pid = std::process::id() as i32;
        let own_window = WindowInfo {
            sys_id: Some(WindowServerId::new(50)),
            ..make_window(1)
        };
        s.reactor.handle_events(s.apps.make_app(own_pid, vec![own_window]));
        let on_right = WindowInfo {
            frame: rect(1300., 100., 50., 50.),
            ..make_window(5)
        };
        let mut windows = make_windows(4);
        windows.push(on_right);
        let mut launch = s.apps.make_app(1, windows);
        // Window 4 is a panel on a layer of its own.
        for event in &mut launch {
            if let Event::WindowsOnScreenUpdated { on_screen, .. } = event {
                on_screen.info[3].layer = 1;
            }
        }
        s.reactor.handle_events(launch);
        s.reactor.handle_event(Event::StartupComplete);
        // Window 3 is minimized.
        let visible = |id: u32, layer: i32, frame: CGRect| WindowServerInfo {
            id: WindowServerId::new(id),
            pid: if id == 50 { own_pid } else { 1 },
            layer,
            frame,
        };
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(vec![
                visible(50, 0, rect(100., 100., 50., 50.)),
                visible(1, 0, rect(100., 100., 50., 50.)),
                visible(2, 0, rect(200., 100., 50., 50.)),
                visible(4, 1, rect(400., 100., 50., 50.)),
                visible(5, 0, rect(1300., 100., 50., 50.)),
            ]),
        });
        s.reactor.update_visible_windows();
        s.apps.simulate_until_quiet(&mut s.reactor);

        s.create("Work");

        assert_eq!(vec![wid(1), wid(2)], s.members("Work"));
        assert_eq!(ContextKey::Named(s.id("Work")), s.reactor.contexts.active());
        assert!(s.parked().is_empty());
    }

    /// L3. Under a context, the new context takes the windows that
    /// show. A parked window stays parked and out, and a pinned window gets
    /// no record (R3). The windows keep their frames.
    #[test]
    fn a_new_context_keeps_what_the_screen_shows() {
        let mut s = Setup::new(4);
        let c = s.reactor.contexts.create("C").unwrap();
        let d = s.reactor.contexts.create("D").unwrap();
        for (id, idx) in [(c, 1), (c, 2), (d, 3)] {
            let desc = s.reactor.window_desc(wid(idx)).unwrap();
            s.reactor.contexts.add_window(id, &desc).unwrap();
        }
        let pinned = s.reactor.window_desc(wid(4)).unwrap();
        s.reactor.contexts.pin(&pinned);
        s.run(ContextCommand::SwitchContext(ContextRef::Id(c)));
        assert_eq!(vec![wid(3)], s.parked());
        let shown = [wid(1), wid(2), wid(4)];
        let frames = s.frames(&shown);

        s.create("New");

        assert_eq!(vec![wid(1), wid(2)], s.members("New"));
        assert_eq!(ContextKey::Named(s.id("New")), s.reactor.contexts.active());
        assert_eq!(vec![wid(3)], s.parked());
        assert_eq!(frames, s.frames(&shown));
        let mut tiles = s.reactor.layout.calculate_layout(space(), screen(), &s.reactor.config);
        tiles.sort_by_key(|(wid, _)| *wid);
        assert_eq!(frames, tiles);
        assert_eq!(vec![wid(1), wid(2)], s.members("C"));
    }

    /// The new context and its members are saved, and it is saved active.
    #[test]
    fn a_new_context_is_saved() {
        let mut s = Setup::new(2);

        s.create("Work");

        let saved = s.saved();
        let work = saved.by_name("Work").unwrap();
        assert_eq!(Some(1), work.number);
        assert_eq!(
            vec!["Window1", "Window2"],
            work.members.iter().map(|m| m.title.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(ContextKey::Named(work.id), saved.active());
    }

    /// R4. A name that is taken, reserved, or empty creates nothing and
    /// switches nowhere.
    #[test]
    fn a_new_context_needs_a_free_name() {
        let mut s = Setup::new(2);
        s.create("Work");
        s.run(ContextCommand::ShowEverything);

        for name in ["work", "Everything", "  "] {
            s.create(name);
        }

        assert_eq!(1, s.reactor.contexts.contexts().len());
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
    }

    /// R28. With contexts off, nothing is created.
    #[test]
    fn no_context_is_created_while_contexts_are_off() {
        let mut s = Setup::new(2);
        s.reactor.handle_event(Event::ConfigChanged(config(false)));

        s.create("Work");

        assert!(s.reactor.contexts.contexts().is_empty());
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
    }

    /// Sends `command` to the message server as `sugarglider context` does,
    /// with request id `request`, while the server has the snapshot
    /// `published`. Returns the server's reply and what it sends on.
    pub fn send_to_server(
        published: &ContextsSnapshot,
        request: u64,
        command: ContextCommand,
    ) -> (Response, Option<(RequestId, ContextCommand)>) {
        let request = ContextRequest::Run(RequestId(request), command);
        answer_context_request(request, Some(published))
    }

    /// Runs what the server sent on, as the window manager passes it to the
    /// reactor, and lets the apps answer.
    pub fn run_sent(s: &mut Setup, sent: Option<(RequestId, ContextCommand)>) {
        let (request, command) = sent.expect("the server sends the command");
        s.reactor.handle_event(Event::ContextCommandRequested(request, command));
        s.apps.simulate_until_quiet(&mut s.reactor);
    }

    /// The server's reply when the command line asks for the result of
    /// `request`, from the snapshot the reactor published last.
    pub fn result_of(s: &Setup, request: u64) -> Response {
        let snapshot = s.reactor.published_contexts.as_deref();
        answer_context_request(ContextRequest::Result(RequestId(request)), snapshot).0
    }

    /// I3. `sugarglider context create Work` followed at once by
    /// `sugarglider context switch Work`, while "Workshop" exists. The server
    /// answers both from a snapshot that doesn't have Work yet, where "Work"
    /// starts Workshop's name. It sends both commands as they were written,
    /// and the reactor resolves the name after it has created Work.
    #[test]
    fn a_switch_right_after_a_create_goes_to_the_new_context() {
        let mut s = Setup::new(2);
        s.create("Workshop");
        s.run(ContextCommand::ShowEverything);
        let stale = s.reactor.published_contexts.clone().unwrap();
        let by_name = ContextCommand::SwitchContext(ContextRef::Name("Work".into()));

        let (reply, create) =
            send_to_server(&stale, 1, ContextCommand::CreateContext("Work".into()));
        assert_eq!(Response::Success, reply);
        let (reply, switch) = send_to_server(&stale, 2, by_name.clone());
        assert_eq!(
            (Response::Success, Some((RequestId(2), by_name))),
            (reply, switch.clone())
        );
        assert_eq!(Response::Pending, result_of(&s, 1));

        run_sent(&mut s, create);
        let work = ContextKey::Named(s.id("Work"));
        let created = s.reactor.contexts.last_used(work);
        run_sent(&mut s, switch);

        assert_eq!(work, s.reactor.contexts.active());
        assert_eq!(created + 1, s.reactor.contexts.last_used(work));
        assert_eq!(vec![wid(1), wid(2)], s.members("Work"));
        assert_eq!(
            (Response::Success, Response::Success),
            (result_of(&s, 1), result_of(&s, 2))
        );
    }

    /// The command survives the RON round trip that recordings use.
    #[test]
    fn the_create_command_survives_a_ron_round_trip() {
        let event = context(ContextCommand::CreateContext("Client work".into()));
        let ron = ron::ser::to_string(&event).unwrap();
        assert_eq!("Command(create_context(\"Client work\"))", ron);
        let Event::Command(Command::Context(command)) = ron::de::from_str(&ron).unwrap() else {
            panic!("{ron}");
        };
        assert_eq!(ContextCommand::CreateContext("Client work".into()), command);
    }

    /// Makes app 1's window float at the frame it had before it was tiled.
    fn float(s: &mut Setup, idx: u32) {
        s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
        s.reactor.send_layout_event(LayoutEvent::WindowFocused(vec![space()], wid(idx)));
        s.reactor.handle_event(Event::Command(Command::Layout(
            LayoutCommand::ToggleWindowFloating,
        )));
        s.apps.simulate_until_quiet(&mut s.reactor);
    }

    fn tiles(s: &Setup, space: SpaceId, screen: CGRect) -> Vec<(WindowId, CGRect)> {
        let mut tiles = s.reactor.layout.calculate_layout(space, screen, &s.reactor.config);
        tiles.sort_by_key(|(wid, _)| *wid);
        tiles
    }

    /// Launches app `pid` with `windows`. Its window server snapshot also
    /// lists the windows in `also` at their frames, since the window server
    /// always lists every window on screen.
    fn launch(s: &mut Setup, pid: i32, windows: Vec<WindowInfo>, also: &[WindowId]) {
        let mut listed: Vec<WindowServerInfo> = also
            .iter()
            .map(|&wid| WindowServerInfo {
                id: s.reactor.windows[&wid].window_server_id.unwrap(),
                pid: wid.pid,
                layer: 0,
                frame: s.apps.windows[&wid].frame,
            })
            .collect();
        let mut events = s.apps.make_app(pid, windows);
        for event in &mut events {
            if let Event::WindowsOnScreenUpdated { on_screen, .. } = event {
                listed.append(&mut on_screen.info);
                *on_screen = WindowsOnScreen::new(std::mem::take(&mut listed));
            }
        }
        s.reactor.handle_events(events);
        s.apps.simulate_until_quiet(&mut s.reactor);
    }

    /// L7. A window the user floats and a window that floats by default
    /// are tracked windows on screen, so the new context holds them. They
    /// stay where they float, and the tiles don't change.
    #[test]
    fn a_new_context_holds_the_floating_windows_where_they_float() {
        let mut s = Setup::new(3);
        float(&mut s, 1);
        let fixed = WindowId::new(2, 1);
        let fixed_window = WindowInfo {
            is_resizable: false,
            sys_id: Some(WindowServerId::new(21)),
            frame: rect(500., 500., 80., 80.),
            ..make_window(1)
        };
        launch(&mut s, 2, vec![fixed_window], &[wid(1), wid(2), wid(3)]);
        let all = [wid(1), wid(2), wid(3), fixed];
        let before = vec![
            (wid(1), rect(100., 100., 50., 50.)),
            (wid(2), rect(0., 0., 600., 1000.)),
            (wid(3), rect(600., 0., 600., 1000.)),
            (fixed, rect(500., 500., 80., 80.)),
        ];
        assert_eq!(before, s.frames(&all));
        let tiled = vec![before[1], before[2]];
        assert_eq!(tiled, tiles(&s, space(), screen()));

        s.create("Work");

        assert_eq!(all.to_vec(), s.members("Work"));
        assert_eq!(ContextKey::Named(s.id("Work")), s.reactor.contexts.active());
        assert!(s.parked().is_empty());
        assert_eq!(before, s.frames(&all));
        assert_eq!(tiled, tiles(&s, space(), screen()));

        s.run(ContextCommand::ShowEverything);
        s.run(ContextCommand::SwitchContext(ContextRef::Name("Work".into())));
        assert_eq!(before, s.frames(&all));
        assert!(s.parked().is_empty());
    }

    /// R1. Under Everything, the new context takes every window on
    /// screen, also the windows that are in other contexts, and those
    /// contexts keep them. The window of an app the user hid is not on
    /// screen and stays out.
    #[test]
    fn a_new_context_takes_windows_that_other_contexts_hold_too() {
        let mut s = Setup::new(4);
        let hidden = WindowId::new(2, 1);
        let hidden_window = WindowInfo {
            sys_id: Some(WindowServerId::new(21)),
            ..make_window(5)
        };
        launch(&mut s, 2, vec![hidden_window], &[wid(1), wid(2), wid(3), wid(4)]);
        let c = s.reactor.contexts.create("C").unwrap();
        let d = s.reactor.contexts.create("D").unwrap();
        for (id, idx) in [(c, 1), (c, 2), (d, 2), (d, 3)] {
            let desc = s.reactor.window_desc(wid(idx)).unwrap();
            s.reactor.contexts.add_window(id, &desc).unwrap();
        }
        // App 2 is hidden, so the window server lists only app 1's windows.
        let listed: Vec<WindowServerInfo> = (1..=4)
            .map(|idx| WindowServerInfo {
                id: WindowServerId::new(idx),
                pid: 1,
                layer: 0,
                frame: s.apps.windows[&wid(idx)].frame,
            })
            .collect();
        s.reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(listed),
        });
        s.reactor.update_visible_windows();
        s.apps.simulate_until_quiet(&mut s.reactor);
        let shown = [wid(1), wid(2), wid(3), wid(4)];
        let frames = vec![
            (wid(1), rect(0., 0., 300., 1000.)),
            (wid(2), rect(300., 0., 300., 1000.)),
            (wid(3), rect(600., 0., 300., 1000.)),
            (wid(4), rect(900., 0., 300., 1000.)),
        ];
        assert_eq!(frames, s.frames(&shown));
        let hidden_frame = s.apps.windows[&hidden].frame;

        s.create("New");

        assert_eq!(shown.to_vec(), s.members("New"));
        assert_eq!(vec![wid(1), wid(2)], s.members("C"));
        assert_eq!(vec![wid(2), wid(3)], s.members("D"));
        assert!(s.parked().is_empty());
        assert_eq!(frames, s.frames(&shown));
        assert_eq!(frames, tiles(&s, space(), screen()));
        assert_eq!(hidden_frame, s.apps.windows[&hidden].frame);

        s.run(ContextCommand::SwitchContext(ContextRef::Id(d)));
        assert_eq!(vec![wid(1), wid(4)], s.parked());
        s.run(ContextCommand::SwitchContext(ContextRef::Name("New".into())));
        assert!(s.parked().is_empty());
        assert_eq!(frames, s.frames(&shown));
    }

    /// R7, L3. In global scope the new context takes the windows on
    /// every visible Space, and each Space keeps its arrangement.
    #[test]
    fn a_new_context_takes_the_windows_on_both_displays() {
        let right = rect(1200., 0., 1200., 1000.);
        let space2 = SpaceId::new(2);
        let mut s = Setup::on(vec![screen(), right], vec![Some(space()), Some(space2)]);
        let mut windows = make_windows(3);
        windows[1].frame = rect(1300., 100., 50., 50.);
        s.reactor.handle_events(s.apps.make_app(1, windows));
        s.reactor.handle_event(Event::StartupComplete);
        s.apps.simulate_until_quiet(&mut s.reactor);
        let all = [wid(1), wid(2), wid(3)];
        let frames = vec![
            (wid(1), rect(0., 0., 600., 1000.)),
            (wid(2), right),
            (wid(3), rect(600., 0., 600., 1000.)),
        ];
        assert_eq!(frames, s.frames(&all));

        s.create("Both");

        assert_eq!(all.to_vec(), s.members("Both"));
        assert!(s.parked().is_empty());
        assert_eq!(frames, s.frames(&all));
        assert_eq!(vec![frames[0], frames[2]], tiles(&s, space(), screen()));
        assert_eq!(vec![frames[1]], tiles(&s, space2, right));
    }

    /// With no window on screen, the new context has no members, and
    /// it still becomes active and is saved.
    #[test]
    fn a_new_context_with_no_window_on_screen_is_empty_and_active() {
        let mut s = Setup::on(vec![screen()], vec![Some(space())]);
        s.reactor.handle_event(Event::StartupComplete);

        s.create("Empty");

        let empty = s.reactor.contexts.by_name("Empty").unwrap();
        assert!(empty.members.is_empty());
        assert_eq!(Some(1), empty.number);
        assert_eq!(ContextKey::Named(empty.id), s.reactor.contexts.active());
        assert_eq!(ContextKey::Named(empty.id), s.saved().active());
        assert!(s.saved().by_name("Empty").unwrap().members.is_empty());
    }

    /// R3, R29. Under Unsorted the new context takes the unsorted
    /// windows. The pinned window shows there too, gets no record, and
    /// still shows under the new context. The parked member of C stays
    /// out.
    #[test]
    fn a_new_context_under_unsorted_takes_the_unsorted_windows() {
        let mut s = Setup::new(4);
        let c = s.reactor.contexts.create("C").unwrap();
        let desc = s.reactor.window_desc(wid(1)).unwrap();
        s.reactor.contexts.add_window(c, &desc).unwrap();
        let pinned = s.reactor.window_desc(wid(4)).unwrap();
        s.reactor.contexts.pin(&pinned);
        s.run(ContextCommand::SwitchContext(ContextRef::Name(
            "Unsorted".into(),
        )));
        assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
        assert_eq!(vec![wid(1)], s.parked());
        let shown = [wid(2), wid(3), wid(4)];
        let frames = s.frames(&shown);

        s.create("New");

        let new = ContextKey::Named(s.id("New"));
        assert_eq!(vec![wid(2), wid(3)], s.members("New"));
        assert_eq!(new, s.reactor.contexts.active());
        assert!(s.reactor.contexts.is_member(new, wid(4)));
        assert_eq!(1, s.reactor.contexts.pinned().len());
        assert_eq!(vec![wid(1)], s.parked());
        assert_eq!(vec![wid(1)], s.members("C"));
        assert_eq!(frames, s.frames(&shown));
        assert_eq!(frames, tiles(&s, space(), screen()));
    }

    /// R4. The name is trimmed. A name that differs from a taken or reserved
    /// one only in case or accents creates nothing and switches nowhere.
    #[test]
    fn a_new_context_name_is_trimmed_and_compared_without_case_or_accents() {
        let mut s = Setup::new(2);

        s.create("  Work  ");
        let work = s.id("Work");
        assert_eq!("Work", s.reactor.contexts.get(work).unwrap().name);
        for name in ["WÖRK", " work", "Évérything", "UNSORTED", "", "\t"] {
            s.create(name);
        }

        let names: Vec<&str> =
            s.reactor.contexts.contexts().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(vec!["Work"], names);
        assert_eq!(ContextKey::Named(work), s.reactor.contexts.active());
        assert_eq!(
            vec!["Work"],
            s.saved().contexts().iter().map(|c| c.name.as_str()).collect::<Vec<_>>()
        );
    }

    /// R32, R5. While a quit waits for parked windows to come back, a new
    /// context is ignored. After nine numbered contexts, a new one gets no
    /// number.
    #[test]
    fn a_new_context_is_ignored_while_quitting_and_takes_the_lowest_free_number() {
        let mut s = Setup::new(2);
        let c = s.reactor.contexts.create("C").unwrap();
        let desc = s.reactor.window_desc(wid(1)).unwrap();
        s.reactor.contexts.add_window(c, &desc).unwrap();
        s.run(ContextCommand::SwitchContext(ContextRef::Id(c)));
        assert_eq!(vec![wid(2)], s.parked());
        s.reactor
            .handle_event(Event::Command(Command::Reactor(ReactorCommand::SaveAndExit)));
        assert!(s.reactor.pending_exit.is_some());
        let unparking = s.apps.requests();
        assert!(!unparking.is_empty());

        s.reactor.handle_event(context(ContextCommand::CreateContext("Work".into())));

        assert!(s.apps.requests().is_empty());
        assert_eq!(1, s.reactor.contexts.contexts().len());
        assert_eq!(ContextKey::Named(c), s.reactor.contexts.active());

        let mut s = Setup::new(1);
        for name in ["A", "B", "C", "D", "E", "F", "G", "H", "I"] {
            s.reactor.contexts.create(name).unwrap();
        }
        s.create("Tenth");
        assert_eq!(None, s.reactor.contexts.by_name("Tenth").unwrap().number);
        assert_eq!(ContextKey::Named(s.id("Tenth")), s.reactor.contexts.active());
        let e = s.id("E");
        s.reactor.delete_context(e).unwrap();
        s.create("Eleventh");
        assert_eq!(Some(5), s.reactor.contexts.by_name("Eleventh").unwrap().number);
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

    /// While no screen shows a managed Space, as at the login window,
    /// `create` makes no context and says why. So the next Space change
    /// parks nothing.
    #[test]
    fn a_new_context_is_refused_while_no_space_is_managed() {
        let mut s = Setup::new(2);
        let all = listed(&s, &[1, 2]);
        s.reactor.handle_event(Event::SpaceChanged(vec![None], all.clone()));
        s.apps.simulate_until_quiet(&mut s.reactor);
        let unmanaged = s.reactor.published_contexts.clone().unwrap();
        assert!(unmanaged.screens.is_empty());

        let (reply, create) =
            send_to_server(&unmanaged, 1, ContextCommand::CreateContext("Work".into()));
        assert_eq!(Response::Success, reply);
        run_sent(&mut s, create);

        assert_eq!(
            Response::Error("No Space is managed right now".into()),
            result_of(&s, 1)
        );
        assert!(s.reactor.contexts.contexts().is_empty());
        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space())], all));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert!(s.parked().is_empty());
        assert!(s.reactor.contexts.contexts().is_empty());
    }

    /// R33. After `sugarglider pause`, which shows every window and then
    /// leaves no Space managed, a switch, Show Everything, and the previous
    /// context do nothing and say why. The active context stays, so resuming
    /// shows what showed before the pause.
    #[test]
    fn a_switch_is_refused_while_no_space_is_managed() {
        let mut s = Setup::new(2);
        let work = s.reactor.contexts.create("Work").unwrap();
        let desc = s.reactor.window_desc(wid(1)).unwrap();
        s.reactor.contexts.add_window(work, &desc).unwrap();
        s.run(ContextCommand::SwitchContext(ContextRef::Id(work)));
        s.run(ContextCommand::ShowEverything);
        assert!(s.parked().is_empty());
        let all = listed(&s, &[1, 2]);
        s.reactor.handle_event(Event::ShowEverythingOn(vec![space()]));
        s.reactor.handle_event(Event::SpaceChanged(vec![None], all.clone()));
        s.apps.simulate_until_quiet(&mut s.reactor);
        let paused = s.reactor.published_contexts.clone().unwrap();

        for (request, command) in [
            (1, ContextCommand::SwitchContext(ContextRef::Name("Work".into()))),
            (2, ContextCommand::ShowEverything),
            (3, ContextCommand::PreviousContext),
        ] {
            let (_, sent) = send_to_server(&paused, request, command);
            run_sent(&mut s, sent);
            assert_eq!(
                Response::Error("No Space is managed right now".into()),
                result_of(&s, request)
            );
        }

        assert_eq!(ContextKey::Everything, s.reactor.contexts.active());
        s.reactor.handle_event(Event::SpaceChanged(vec![Some(space())], all));
        s.apps.simulate_until_quiet(&mut s.reactor);
        assert!(s.parked().is_empty());
        s.run(ContextCommand::PreviousContext);
        assert_eq!(ContextKey::Named(work), s.reactor.contexts.active());
        assert_eq!(vec![wid(2)], s.parked());
    }

    /// I3. The switch after a create names the new context in another
    /// case and with an accent. The reactor resolves it when it runs it.
    #[test]
    fn a_switch_right_after_a_create_finds_the_new_context_in_another_spelling() {
        let mut s = Setup::new(2);
        s.create("Comms");
        s.run(ContextCommand::ShowEverything);
        let stale = s.reactor.published_contexts.clone().unwrap();
        let by_name = ContextCommand::SwitchContext(ContextRef::Name("CLIENT WÖRK".into()));

        let (_, create) =
            send_to_server(&stale, 1, ContextCommand::CreateContext("Client work".into()));
        let (reply, switch) = send_to_server(&stale, 2, by_name.clone());
        assert_eq!(
            (Response::Success, Some((RequestId(2), by_name))),
            (reply, switch.clone())
        );
        run_sent(&mut s, create);
        s.run(ContextCommand::ShowEverything);
        run_sent(&mut s, switch);

        assert_eq!(
            ContextKey::Named(s.id("Client work")),
            s.reactor.contexts.active()
        );
        assert_eq!(Response::Success, result_of(&s, 2));
    }

    /// I3. The server sends a number as it was written too, so a number that
    /// the stale snapshot doesn't have names the new context once the
    /// reactor has created it.
    #[test]
    fn a_number_right_after_a_create_names_the_new_context() {
        let mut s = Setup::new(2);
        s.create("Comms");
        let stale = s.reactor.published_contexts.clone().unwrap();
        let number = ContextCommand::SwitchContext(ContextRef::Number(2));

        let (_, create) =
            send_to_server(&stale, 1, ContextCommand::CreateContext("Client work".into()));
        let (reply, switch) = send_to_server(&stale, 2, number.clone());
        assert_eq!(
            (Response::Success, Some((RequestId(2), number))),
            (reply, switch.clone())
        );
        run_sent(&mut s, create);
        s.run(ContextCommand::SwitchContext(ContextRef::Name("Comms".into())));
        run_sent(&mut s, switch);

        let client = s.id("Client work");
        assert_eq!(Some(2), s.reactor.contexts.get(client).unwrap().number);
        assert_eq!(ContextKey::Named(client), s.reactor.contexts.active());
        assert_eq!(Response::Success, result_of(&s, 2));
    }

    /// The reactor resolves a name when it runs the command, so a switch by
    /// name goes to the context that has the name then, also after a
    /// rename that came between the request and the switch.
    #[test]
    fn a_switch_by_name_goes_to_the_context_that_has_the_name_when_it_runs() {
        let mut s = Setup::new(2);
        s.create("Client work");
        s.create("Comms");
        let snapshot = s.reactor.published_contexts.clone().unwrap();
        let by_name = ContextCommand::SwitchContext(ContextRef::Name("cli".into()));
        let (_, switch) = send_to_server(&snapshot, 1, by_name);
        let client = s.id("Client work");
        let comms = s.id("Comms");

        s.reactor.contexts.rename(client, "Old clients").unwrap();
        s.reactor.contexts.rename(comms, "Client work").unwrap();
        s.run(ContextCommand::SwitchContext(ContextRef::Id(client)));
        run_sent(&mut s, switch);

        assert_eq!(ContextKey::Named(comms), s.reactor.contexts.active());
    }

    /// I3. `sugarglider context create Work` followed at once by
    /// `sugarglider context switch Work`, while "Client work" exists. The
    /// stale snapshot doesn't have Work, and "Work" starts a word of "Client
    /// work".
    #[test]
    fn a_switch_right_after_a_create_goes_to_the_new_context_when_its_name_matches_another() {
        let mut s = Setup::new(2);
        s.create("Client work");
        s.run(ContextCommand::ShowEverything);
        let stale = s.reactor.published_contexts.clone().unwrap();

        let (reply, create) =
            send_to_server(&stale, 1, ContextCommand::CreateContext("Work".into()));
        assert_eq!(Response::Success, reply);
        let (reply, switch) = send_to_server(
            &stale,
            2,
            ContextCommand::SwitchContext(ContextRef::Name("Work".into())),
        );
        assert_eq!(Response::Success, reply);
        run_sent(&mut s, create);
        let work = ContextKey::Named(s.id("Work"));
        assert_eq!(work, s.reactor.contexts.active());
        run_sent(&mut s, switch);

        assert_eq!(work, s.reactor.contexts.active());
        assert_eq!(Response::Success, result_of(&s, 2));
    }
}

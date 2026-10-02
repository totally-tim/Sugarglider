// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::create_context::tests::*;
use super::testing::*;
use super::*;
use crate::model::contexts::{ContextKey, Scope, WindowDesc};
use crate::sys::screen::ScreenId;
use crate::sys::window_server::{WindowServerId, WindowServerInfo, WindowsOnScreen};

fn per_screen(s: &mut Setup) {
    let mut config = crate::config::Config::default();
    config.settings.default_disable = false;
    config.settings.animate = false;
    config.settings.experimental.contexts.enable = true;
    config.settings.experimental.contexts.scope = Scope::PerScreen;
    s.reactor.handle_event(Event::ConfigChanged(Arc::new(config)));
}

#[test]
fn per_screen_switcher_marks_the_focused_screens_context() {
    let mut s = Setup::on(vec![screen()], vec![Some(space())]);
    s.reactor.handle_event(Event::DisplayIdsChanged(vec![1]));
    per_screen(&mut s);
    s.reactor.handle_events(s.apps.make_app(1, make_windows(1)));
    s.reactor.handle_event(Event::StartupComplete);
    let id = s.reactor.contexts.create("Work").unwrap();
    let desc = s.reactor.window_desc(wid(1)).unwrap();
    s.reactor.contexts.add_window(id, &desc).unwrap();
    s.reactor.switch_context_on(Some(0), ContextKey::Named(id), None).unwrap();
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(ContextKey::Named(id), s.reactor.screen_key(0));
    let payload = s.reactor.switcher_payload();
    assert!(
        payload.contexts.iter().find(|c| c.id == id.get()).unwrap().active,
        "Work must be active in the panel when the focused display shows Work: {payload:?}"
    );
    assert!(!payload.everything.active);
}

#[test]
fn arrival_matching_runs_exact_pass_across_both_screens_first() {
    let mut s = Setup::on(
        vec![screen(), rect(1200., 0., 1200., 1000.)],
        vec![Some(space()), Some(SpaceId::new(2))],
    );
    per_screen(&mut s);
    let left = s.reactor.contexts.create("Left").unwrap();
    let right = s.reactor.contexts.create("Right").unwrap();
    let saved = s.reactor.contexts.create("Saved").unwrap();
    s.reactor
        .contexts
        .switch_to_on(ScreenId::new(1), ContextKey::Named(left))
        .unwrap();
    s.reactor
        .contexts
        .switch_to_on(ScreenId::new(2), ContextKey::Named(right))
        .unwrap();
    let record = WindowDesc {
        wid: WindowId::new(1, 99),
        bundle_id: Some("com.testapp1".into()),
        app_name: Some("TestApp1".into()),
        title: "Project draft".into(),
        window_server_id: Some(WindowServerId::new(99)),
    };
    s.reactor.contexts.add_window(saved, &record).unwrap();
    s.reactor.contexts.window_closed(record.wid);
    s.reactor.contexts.app_terminated(1);
    s.reactor.handle_event(Event::StartupComplete);
    let mut windows = vec![make_window(1), make_window(2)];
    windows[0].title = "Project drafX".to_owned().into();
    windows[0].frame = rect(100., 100., 200., 200.);
    windows[1].title = "Project draft".to_owned().into();
    windows[1].frame = rect(1300., 100., 200., 200.);
    s.reactor.handle_events(s.apps.make_app(1, windows));
    assert_eq!(
        Some(wid(2)),
        s.reactor.contexts.get(saved).unwrap().members[0].window(),
        "The exact title on the right must beat the similar title on the left"
    );
}

#[test]
fn per_screen_switcher_lists_only_the_focused_displays_windows() {
    let mut s = Setup::on(
        vec![screen(), rect(1200., 0., 1200., 1000.)],
        vec![Some(space()), Some(SpaceId::new(2))],
    );
    per_screen(&mut s);
    let mut windows = vec![make_window(1), make_window(2)];
    windows[0].frame = rect(100., 100., 200., 200.);
    windows[1].frame = rect(1300., 100., 200., 200.);
    s.reactor.handle_events(s.apps.make_app(1, windows));
    assert_eq!(0, s.reactor.focused_screen_index());
    let listed: Vec<_> = s.reactor.switcher_payload().windows.iter().map(|w| w.id).collect();
    assert_eq!(
        vec![wid(1)],
        listed,
        "The create/edit picker must not include windows on the other display in per_screen scope"
    );
}

#[test]
fn enabling_contexts_and_changing_scope_preserves_the_global_context() {
    let mut s = Setup::new(1);
    s.reactor.handle_event(Event::StartupComplete);
    let work = s.reactor.contexts.create("Work").unwrap();
    let desc = s.reactor.window_desc(wid(1)).unwrap();
    s.reactor.contexts.add_window(work, &desc).unwrap();
    s.reactor.switch_context(ContextKey::Named(work)).unwrap();
    s.apps.simulate_until_quiet(&mut s.reactor);
    let mut off = crate::config::Config::default();
    off.settings.default_disable = false;
    off.settings.animate = false;
    off.settings.experimental.contexts.enable = false;
    off.settings.experimental.contexts.scope = Scope::Global;
    s.reactor.handle_event(Event::ConfigChanged(Arc::new(off)));
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(ContextKey::Named(work), s.reactor.contexts.active());
    per_screen(&mut s); // enable = true and scope = per_screen in one reload
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(
        ContextKey::Named(work),
        s.reactor.screen_key(0),
        "R11 gives every screen the former global context when enabling per_screen scope"
    );
}

#[test]
fn display_removal_moves_focus_off_a_window_it_parks() {
    let mut s = Setup::on(
        vec![screen(), rect(1200., 0., 1200., 1000.)],
        vec![Some(space()), Some(SpaceId::new(2))],
    );
    per_screen(&mut s);
    let left = s.reactor.contexts.create("Left").unwrap();
    let right = s.reactor.contexts.create("Right").unwrap();
    s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
    s.reactor
        .handle_events(s.apps.make_app_with_opts(1, make_windows(1), Some(wid(1)), true));
    s.reactor.handle_events(s.apps.make_app(2, vec![]));
    s.reactor.apps.get_mut(&2).unwrap().info.bundle_id = Some("com.apple.finder".into());
    let desc = s.reactor.window_desc(wid(1)).unwrap();
    s.reactor.contexts.add_window(left, &desc).unwrap();
    s.reactor
        .contexts
        .switch_to_on(ScreenId::new(1), ContextKey::Named(left))
        .unwrap();
    s.reactor
        .contexts
        .switch_to_on(ScreenId::new(2), ContextKey::Named(right))
        .unwrap();
    s.reactor.handle_event(Event::StartupComplete);
    s.apps.simulate_until_quiet(&mut s.reactor);
    assert_eq!(Some(wid(1)), s.reactor.main_window());
    s.apps.requests();
    // macOS moved the focused window onto the surviving display.
    s.reactor.handle_event(Event::ScreenParametersChanged {
        ids: vec![ScreenId::new(2)],
        frames: vec![screen()],
        bounds: vec![screen()],
        spaces: vec![Some(SpaceId::new(2))],
        scale_factors: vec![1.0],
        converter: CoordinateConverter::default(),
        on_screen: WindowsOnScreen::new(vec![WindowServerInfo {
            id: WindowServerId::new(1),
            pid: 1,
            layer: 0,
            frame: rect(100., 100., 200., 200.),
        }]),
    });
    assert!(
        s.reactor.parked.contains_key(&wid(1)),
        "Right must park the Left-only member"
    );
    let requests = s.apps.requests();
    assert!(requests.iter().any(|request| matches!(request, Request::Activate(Quiet::Yes))));
    assert_eq!(Some(2), s.reactor.switch_guard.finder);
    let sent = format!("{requests:?}");
    s.reactor.handle_events(s.apps.simulate_events_for_requests(requests));
    // The harness leaves activation notifications for each test to send.
    s.reactor.handle_event(Event::ApplicationGloballyDeactivated(1));
    s.reactor.handle_event(Event::ApplicationDeactivated(1));
    s.reactor.handle_event(Event::ApplicationGloballyActivated(2));
    s.reactor.handle_event(Event::ApplicationActivated(2, Quiet::Yes));
    s.apps.simulate_until_quiet(&mut s.reactor);
    let main = s.reactor.main_window();
    assert!(
        main.is_none_or(|wid| !s.reactor.parked.contains_key(&wid)),
        "After confirmed frame writes, focus still names a parked window: main={main:?}, requests={sent}"
    );
}

fn reload(s: &mut Setup, enabled: bool, scope: Scope) {
    let mut config = crate::config::Config::default();
    config.settings.default_disable = false;
    config.settings.animate = false;
    config.settings.experimental.contexts.enable = enabled;
    config.settings.experimental.contexts.scope = scope;
    s.reactor.handle_event(Event::ConfigChanged(Arc::new(config)));
    s.apps.simulate_until_quiet(&mut s.reactor);
}

#[test]
fn scope_edits_while_disabled_preserve_the_active_context_when_reenabled() {
    let mut s = Setup::new(1);
    s.reactor.handle_event(Event::StartupComplete);
    let work = s.reactor.contexts.create("Work").unwrap();
    let desc = s.reactor.window_desc(wid(1)).unwrap();
    s.reactor.contexts.add_window(work, &desc).unwrap();
    s.reactor.switch_context(ContextKey::Named(work)).unwrap();
    s.apps.simulate_until_quiet(&mut s.reactor);
    reload(&mut s, false, Scope::Global);
    reload(&mut s, false, Scope::PerScreen);
    assert_eq!(
        ContextKey::Named(work),
        s.reactor.contexts.active_on(ScreenId::new(1))
    );
    assert!(s.reactor.parked.is_empty());
    reload(&mut s, true, Scope::PerScreen);
    assert_eq!(ContextKey::Named(work), s.reactor.screen_key(0));
    reload(&mut s, false, Scope::Global);
    assert!(!s.reactor.contexts.has_screen_actives());
    reload(&mut s, true, Scope::Global);
    assert_eq!(ContextKey::Named(work), s.reactor.screen_key(0));
}

#[test]
fn unread_contexts_are_preserved_through_disabled_scope_edits_and_loaded_before_enabling() {
    let mut s = Setup::new(1);
    s.reactor.handle_event(Event::StartupComplete);
    let work = s.reactor.contexts.create("Work").unwrap();
    let desc = s.reactor.window_desc(wid(1)).unwrap();
    s.reactor.contexts.add_window(work, &desc).unwrap();
    s.reactor.switch_context(ContextKey::Named(work)).unwrap();
    s.apps.simulate_until_quiet(&mut s.reactor);
    reload(&mut s, false, Scope::Global);
    s.reactor.save_contexts();
    let path = s.dir.path().join("contexts.json");
    let saved = std::fs::read(&path).unwrap();
    s.reactor.contexts = crate::model::contexts::Contexts::new();
    s.reactor.contexts_unread = true;
    reload(&mut s, false, Scope::PerScreen);
    assert_eq!(saved, std::fs::read(&path).unwrap());
    reload(&mut s, false, Scope::Global);
    assert_eq!(saved, std::fs::read(&path).unwrap());
    reload(&mut s, true, Scope::PerScreen);
    assert_eq!(ContextKey::Named(work), s.reactor.screen_key(0));
    assert_eq!(
        Some(wid(1)),
        s.reactor.contexts.get(work).unwrap().members[0].window()
    );
}

#[test]
fn space_change_activates_finder_when_it_parks_the_focused_nonmember() {
    let mut s = Setup::on(vec![screen()], vec![Some(space())]);
    s.reactor.handle_event(Event::ApplicationGloballyActivated(1));
    s.reactor
        .handle_events(s.apps.make_app_with_opts(1, make_windows(1), Some(wid(1)), true));
    s.reactor.handle_events(s.apps.make_app(2, vec![]));
    s.reactor.apps.get_mut(&2).unwrap().info.bundle_id = Some("com.apple.finder".into());
    s.reactor.handle_event(Event::StartupComplete);
    let empty = s.reactor.contexts.create("Empty").unwrap();
    s.reactor.contexts.switch_to(ContextKey::Named(empty)).unwrap();
    s.apps.requests();
    s.reactor.handle_event(Event::SpaceChanged(
        vec![Some(SpaceId::new(2))],
        WindowsOnScreen::new(vec![WindowServerInfo {
            id: WindowServerId::new(1),
            pid: 1,
            layer: 0,
            frame: rect(100., 100., 200., 200.),
        }]),
    ));
    assert!(s.reactor.parked.contains_key(&wid(1)));
    assert!(
        s.apps
            .requests()
            .iter()
            .any(|request| matches!(request, Request::Activate(Quiet::Yes)))
    );
    assert_eq!(Some(2), s.reactor.switch_guard.finder);
}

#[test]
fn global_switcher_keeps_windows_from_both_displays() {
    let mut s = Setup::on(
        vec![screen(), rect(1200., 0., 1200., 1000.)],
        vec![Some(space()), Some(SpaceId::new(2))],
    );
    let mut windows = vec![make_window(1), make_window(2)];
    windows[0].frame = rect(100., 100., 200., 200.);
    windows[1].frame = rect(1300., 100., 200., 200.);
    s.reactor.handle_events(s.apps.make_app(1, windows));
    assert_eq!(
        vec![wid(1), wid(2)],
        s.reactor.switcher_payload().windows.iter().map(|w| w.id).collect::<Vec<_>>()
    );
}

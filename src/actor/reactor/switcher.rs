// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The context switcher's show payload. The contract is
//! `docs/specs/contexts-switcher-contract.md`; the JSON shapes live in
//! `crate::ui::context_switcher`.

use tracing::info;

use super::{Command, ContextCommand, Reactor};
use crate::actor::app::{WindowId, pid_t};
use crate::actor::contexts_snapshot::{UNKNOWN_APP, app_name};
use crate::actor::wm_controller::WmCommand;
use crate::model::contexts::{Context, ContextKey, Scope};
use crate::ui::context_switcher::{
    ContextPayload, EverythingPayload, MemberPayload, ShowPayload, UnsortedPayload, WindowPayload,
};
use crate::ui::preferences_json::format_hotkey;

impl Reactor {
    /// Shows the switcher panel with the payload of the contexts and the
    /// windows on screen. Rust calls it for every `open_context_switcher`
    /// and doesn't track whether the panel is open: the panel decides
    /// whether that opens or closes it.
    pub(super) fn open_context_switcher(&mut self) -> Result<(), String> {
        let payload = self.switcher_payload();
        match serde_json::to_string(&payload) {
            Ok(json) => {
                info!("Showing the context switcher");
                (self.show_switcher)(json);
                Ok(())
            }
            Err(err) => Err(format!("The switcher's payload can't be written: {err}")),
        }
    }

    /// Hides the switcher panel if it is open.
    pub(super) fn hide_context_switcher(&mut self) {
        (self.hide_switcher)();
    }

    /// The payload of the switcher panel: the contexts, the windows on
    /// screen, and the target window.
    pub(super) fn switcher_payload(&self) -> ShowPayload {
        let screen =
            self.current_main_screen_index().unwrap_or_else(|| self.focused_screen_index());
        let windows = self.switcher_windows(screen);
        let active = self.screen_shown_key(screen);
        ShowPayload {
            display_id: self.display_ids.get(screen).copied(),
            target_window: self.switcher_target_window(&windows),
            contexts: self
                .contexts
                .contexts()
                .iter()
                .map(|context| self.context_payload(context, active))
                .collect(),
            unsorted: UnsortedPayload {
                windows: self.unsorted_windows().len(),
                active: active == ContextKey::Unsorted,
            },
            everything: EverythingPayload {
                active: active == ContextKey::Everything,
                hotkey: self.everything_hotkey(),
            },
            windows,
        }
    }

    /// The window that had focus when the switcher was opened, as its tab
    /// group's main tab. A window that is untracked, parked, or Sugarglider's
    /// own is no target.
    fn switcher_target_window(&self, windows: &[WindowPayload]) -> Option<WindowId> {
        let own_pid = std::process::id() as pid_t;
        self.main_window()
            .filter(|&wid| wid.pid != own_pid && !self.parked.contains_key(&wid))
            .filter(|&wid| {
                self.layout_window_info(wid)
                    .is_some_and(|info| !self.layout.is_untracked(&info))
            })
            .map(|wid| self.membership_window(wid))
            .filter(|wid| windows.iter().any(|window| window.id == *wid))
    }

    /// A named context as the panel's list and edit views read it. The apps
    /// and the window count follow R3: a pinned window is a member of every
    /// context. The records follow the model's order, pinned windows left
    /// out, as the edit view derives membership from them.
    fn context_payload(&self, context: &Context, active: ContextKey) -> ContextPayload {
        let members: Vec<MemberPayload> = context
            .members
            .iter()
            .enumerate()
            .map(|(record, member)| MemberPayload {
                record,
                app: app_name(member),
                title: member.title.clone(),
                window: member.window().map(|wid| self.membership_window(wid)),
            })
            .collect();
        let mut open: Vec<WindowId> = Vec::new();
        let mut apps: Vec<String> = Vec::new();
        for member in context.members.iter().chain(self.contexts.pinned()) {
            let Some(wid) = member.window().map(|wid| self.membership_window(wid)) else {
                continue;
            };
            if open.contains(&wid) {
                continue;
            }
            open.push(wid);
            let app = app_name(member);
            if !apps.contains(&app) {
                apps.push(app);
            }
        }
        ContextPayload {
            id: context.id.get(),
            name: context.name.clone(),
            number: context.number,
            hotkey: self.context_hotkey(ContextKey::Named(context.id)),
            active: active == ContextKey::Named(context.id),
            apps,
            windows: open.len(),
            members,
        }
    }

    /// The windows the create and edit views list: the tracked windows that
    /// show on the visible Spaces, restricted to the focused screen in
    /// per-screen scope. Each native tab group has one entry, named by its
    /// main tab (R36).
    fn switcher_windows(&self, focused: usize) -> Vec<WindowPayload> {
        let mut seen: Vec<WindowId> = Vec::new();
        let mut windows = Vec::new();
        for wid in self.windows_on_visible_spaces(|wid| !self.parked.contains_key(&wid)) {
            if self.scope() == Scope::PerScreen
                && self.layout_frame(wid).and_then(|frame| self.best_screen_idx_for_window(&frame))
                    != Some(focused)
            {
                continue;
            }
            let main = self.membership_window(wid);
            if seen.contains(&main) {
                continue;
            }
            seen.push(main);
            let Some(window) = self.windows.get(&main) else {
                continue;
            };
            windows.push(WindowPayload {
                id: main,
                title: window.title.expose_secret().clone(),
                app: self.app_display_name(main.pid),
                tab_count: self.native_group(main).map_or(1, |group| group.snapshot.members.len()),
                pinned: self.contexts.is_pinned(main),
            });
        }
        windows
    }

    /// The name the panel shows for an app: its localized name, else its
    /// bundle id, else "Unknown app", as the member records derive it.
    fn app_display_name(&self, pid: pid_t) -> String {
        let app = self.apps.get(&pid);
        app.and_then(|app| app.info.localized_name.clone())
            .or_else(|| app.and_then(|app| app.info.bundle_id.clone()))
            .unwrap_or_else(|| UNKNOWN_APP.to_string())
    }

    /// The label of the first `switch_context` binding that resolves to
    /// `key`, in config order, or `None`.
    fn context_hotkey(&self, key: ContextKey) -> Option<String> {
        self.config.keys.iter().find_map(|(hotkey, command)| {
            let WmCommand::ReactorCommand(Command::Context(ContextCommand::SwitchContext(
                reference,
            ))) = command
            else {
                return None;
            };
            (self.resolve(reference).ok() == Some(key)).then(|| format_hotkey(hotkey))
        })
    }

    /// The label of the first binding that runs `show_everything`, in config
    /// order, or `None`.
    fn everything_hotkey(&self) -> Option<String> {
        self.config.keys.iter().find_map(|(hotkey, command)| {
            let WmCommand::ReactorCommand(Command::Context(ContextCommand::ShowEverything)) =
                command
            else {
                return None;
            };
            Some(format_hotkey(hotkey))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::super::create_context::tests::{Setup, context, screen, screens, space};
    use super::super::{ContextCommand, Event};

    #[test]
    fn show_toggles_in_swift_and_commands_hide_the_panel() {
        let mut s = Setup::new(2);
        let shown = Arc::new(Mutex::new(Vec::<String>::new()));
        let hidden = Arc::new(Mutex::new(0usize));
        let shown_sink = shown.clone();
        let hidden_sink = hidden.clone();
        s.reactor.show_switcher = Box::new(move |json| shown_sink.lock().unwrap().push(json));
        s.reactor.hide_switcher = Box::new(move || *hidden_sink.lock().unwrap() += 1);
        s.reactor.handle_event(Event::DisplayIdsChanged(vec![42]));
        s.reactor.handle_event(context(ContextCommand::OpenContextSwitcher));
        s.reactor.handle_event(context(ContextCommand::OpenContextSwitcher));
        let payloads = shown.lock().unwrap();
        assert_eq!(payloads.len(), 2);
        let payload: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert_eq!(payload["display_id"], 42);
        assert_eq!(payload["everything"]["active"], true);
        assert_eq!(payload["windows"].as_array().unwrap().len(), 2);
        if !payload["target_window"].is_null() {
            assert!(
                payload["windows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|window| { window["id"] == payload["target_window"] })
            );
        }
        drop(payloads);

        s.create("Work");
        assert!(*hidden.lock().unwrap() > 0);
        assert_eq!(shown.lock().unwrap().len(), 2);
    }

    #[test]
    fn disabled_spaces_and_contexts_refuse_to_show() {
        let mut s = Setup::on(vec![screen()], vec![None]);
        let shown = Arc::new(Mutex::new(0usize));
        let sink = shown.clone();
        s.reactor.show_switcher = Box::new(move |_| *sink.lock().unwrap() += 1);
        s.reactor.handle_event(context(ContextCommand::OpenContextSwitcher));
        assert_eq!(*shown.lock().unwrap(), 0);
        s.reactor.handle_event(screens(vec![screen()], vec![Some(space())]));
        s.reactor.handle_event(context(ContextCommand::OpenContextSwitcher));
        assert_eq!(*shown.lock().unwrap(), 1);
        s.reactor.handle_event(Event::ConfigChanged(
            super::super::create_context::tests::config(false),
        ));
        s.reactor.handle_event(context(ContextCommand::OpenContextSwitcher));
        assert_eq!(*shown.lock().unwrap(), 1);
    }

    #[test]
    fn pending_exit_hides_and_refuses_to_show() {
        let mut s = Setup::new(2);
        let shown = Arc::new(Mutex::new(0usize));
        let hidden = Arc::new(Mutex::new(0usize));
        let shown_sink = shown.clone();
        let hidden_sink = hidden.clone();
        s.reactor.show_switcher = Box::new(move |_| *shown_sink.lock().unwrap() += 1);
        s.reactor.hide_switcher = Box::new(move || *hidden_sink.lock().unwrap() += 1);
        let id = s.reactor.contexts.create("One").unwrap();
        let desc = s.reactor.window_desc(super::super::create_context::tests::wid(1)).unwrap();
        s.reactor.contexts.add_window(id, &desc).unwrap();
        s.run(ContextCommand::SwitchContext(super::super::ContextRef::Id(id)));
        assert!(!s.parked().is_empty());
        s.reactor.handle_event(context(ContextCommand::OpenContextSwitcher));
        assert_eq!(*shown.lock().unwrap(), 1);
        let hidden_before = *hidden.lock().unwrap();
        s.reactor.save_and_exit(Instant::now());
        assert!(s.reactor.pending_exit.is_some());
        assert!(*hidden.lock().unwrap() > hidden_before);
        s.reactor.handle_event(context(ContextCommand::OpenContextSwitcher));
        assert_eq!(*shown.lock().unwrap(), 1);
    }

    #[test]
    fn add_window_grace_survives_space_change_until_context_switch() {
        use crate::model::contexts::ContextKey;

        let mut s = Setup::new(2);
        let id = s.reactor.contexts.create("Work").unwrap();
        let wid = super::super::create_context::tests::wid(1);
        s.run(ContextCommand::SwitchContext(super::super::ContextRef::Name(
            "Unsorted".into(),
        )));
        s.run(ContextCommand::AddWindow {
            window: Some(wid),
            context: super::super::ContextRef::Id(id),
        });
        assert!(s.reactor.shows_under(ContextKey::Unsorted, wid));
        s.reactor
            .handle_event(Event::SpaceChanged(vec![Some(space())], Default::default()));
        assert!(s.reactor.shows_under(ContextKey::Unsorted, wid));
        s.run(ContextCommand::SwitchContext(super::super::ContextRef::Id(id)));
        assert!(!s.reactor.added_since_switch.contains(&wid));
    }
}

// Copyright The Sugarglider Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The context switcher's JSON contract: the payload the panel shows, the rank
//! result, and the commands the panel sends. The contract is
//! `docs/specs/contexts-switcher-contract.md`.
//!
//! The reactor builds the show payload from its own state and calls the
//! panel through [`crate::ui::swift_bridge`]. The panel calls back into
//! `sugarglider_rank_contexts` and `sugarglider_run_context_command`, which
//! live here: both read only the published snapshot
//! ([`crate::actor::contexts_snapshot`]) and never wait on the reactor or on
//! `WmController`, because the panel calls them on the main thread.

use serde::{Deserialize, Serialize};

use crate::actor::app::WindowId;
use crate::actor::contexts_snapshot::ContextsSnapshot;
use crate::actor::reactor::{ContextCommand, ContextRef, RecordRef};
use crate::model::contexts::{
    ContextId, ContextKey, EVERYTHING_NAME, NameMatch, UNSORTED_NAME, fold, rank_entries,
};

/// The show payload: everything the panel needs to list the contexts, the
/// windows on screen, and the target window. Rust serializes every key,
/// nullable ones included, and Swift ignores the keys it doesn't know.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ShowPayload {
    /// The `CGDirectDisplayID` of the focused screen, or `None` when it
    /// isn't known. The panel centers on the main screen then.
    pub display_id: Option<u32>,
    /// The window that had focus when the reactor handled
    /// `open_context_switcher`, as its tab group's main tab. `None` when no
    /// window had focus, or when the focused one is untracked, parked, or
    /// Sugarglider's own.
    pub target_window: Option<WindowId>,
    /// Every named context, in the model's order.
    pub contexts: Vec<ContextPayload>,
    pub unsorted: UnsortedPayload,
    pub everything: EverythingPayload,
    /// The windows on screen that the create and edit views list, one entry
    /// per native tab group (R36).
    pub windows: Vec<WindowPayload>,
}

/// A named context, as the panel's list and edit views read it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContextPayload {
    pub id: u32,
    pub name: String,
    pub number: Option<u8>,
    /// A label for a key binding that switches to this context, or `None`.
    pub hotkey: Option<String>,
    pub active: bool,
    /// The distinct app names of the context's open member windows, in
    /// member order.
    pub apps: Vec<String>,
    /// How many member windows are open.
    pub windows: usize,
    /// Every member record, in the model's order.
    pub members: Vec<MemberPayload>,
}

/// A member record of a context.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MemberPayload {
    /// The record's index in `Context::members`, as `remove_records` takes
    /// it.
    pub record: usize,
    /// The app name, as [`crate::actor::contexts_snapshot::app_name`]
    /// derives it.
    pub app: String,
    pub title: String,
    /// The record's open window, or `None` when the window is gone.
    pub window: Option<WindowId>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UnsortedPayload {
    /// The number of tracked windows on the visible Spaces that are in no
    /// named context and aren't pinned (R3, R29).
    pub windows: usize,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EverythingPayload {
    pub active: bool,
    /// A label for the binding that runs `show_everything`, or `None`.
    pub hotkey: Option<String>,
}

/// A window on screen: a window without tabs, or the main tab of a native
/// tab group (R36).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WindowPayload {
    pub id: WindowId,
    pub title: String,
    pub app: String,
    /// How many tabs its tab group has, 1 for a window without tabs.
    pub tab_count: usize,
    /// Whether it is pinned (R3).
    pub pinned: bool,
}

/// The rank result: the entries of `model::contexts::rank`, best first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RankEntry {
    pub key: SwitcherKey,
    pub r#match: NameMatch,
}

/// A context the switcher can switch to. A named context is always tagged
/// as `{"id": N}`, never a bare number, so it can't be read as a context
/// number (which runs 1 to 9).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SwitcherKey {
    Named(NamedKey),
    Builtin(BuiltinKey),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamedKey {
    pub id: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinKey {
    Everything,
    Unsorted,
}

impl SwitcherKey {
    fn context_key(&self) -> ContextKey {
        match self {
            Self::Named(NamedKey { id }) => ContextKey::Named(ContextId::from_raw(*id)),
            Self::Builtin(BuiltinKey::Everything) => ContextKey::Everything,
            Self::Builtin(BuiltinKey::Unsorted) => ContextKey::Unsorted,
        }
    }
}

/// A command sent by the Swift panel. Named targets are tagged ids, never
/// numbers or names that could resolve to a different context later.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitcherCommand {
    Switch(SwitcherKey),
    AddWindow {
        window: WindowId,
        context: NamedKey,
    },
    MoveWindow {
        window: WindowId,
        context: NamedKey,
    },
    TogglePinned {
        window: WindowId,
    },
    Create {
        name: String,
        windows: Vec<WindowId>,
    },
    Edit {
        context: NamedKey,
        add: Vec<WindowId>,
        remove: Vec<WindowId>,
        remove_records: Vec<RecordRef>,
    },
    Rename {
        context: NamedKey,
        name: String,
    },
    SetNumber {
        context: NamedKey,
        number: u8,
    },
    Delete(NamedKey),
}

/// Ranks only the published snapshot. The FFI caller runs on the main thread.
pub fn rank_snapshot(query: &str, snapshot: &ContextsSnapshot) -> Vec<RankEntry> {
    let entries = snapshot
        .contexts
        .iter()
        .map(|context| {
            (
                ContextKey::Named(context.id),
                context.name.clone(),
                context.last_used,
            )
        })
        .chain(snapshot.unsorted.listed.then(|| {
            (
                ContextKey::Unsorted,
                UNSORTED_NAME.to_string(),
                snapshot.unsorted.last_used,
            )
        }))
        .chain([(
            ContextKey::Everything,
            EVERYTHING_NAME.to_string(),
            snapshot.everything.last_used,
        )]);
    rank_entries(query, entries)
        .into_iter()
        .map(|(key, found)| RankEntry {
            key: match key {
                ContextKey::Named(id) => SwitcherKey::Named(NamedKey { id: id.get() }),
                ContextKey::Everything => SwitcherKey::Builtin(BuiltinKey::Everything),
                ContextKey::Unsorted => SwitcherKey::Builtin(BuiltinKey::Unsorted),
            },
            r#match: found,
        })
        .collect()
}

impl SwitcherCommand {
    /// Rejects stale ids, invalid names and record references before sending.
    pub fn checked(self, snapshot: &ContextsSnapshot) -> Result<ContextCommand, String> {
        let named = |key: NamedKey| -> Result<ContextId, String> {
            let id = ContextId::from_raw(key.id);
            snapshot.get(id).ok_or_else(|| {
                "That context no longer exists. Open the switcher again.".to_string()
            })?;
            Ok(id)
        };
        let name = |value: &str, renaming: Option<ContextId>| -> Result<String, String> {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err("A context name can't be empty".to_string());
            }
            let folded = fold(trimmed);
            if folded == fold(EVERYTHING_NAME) || folded == fold(UNSORTED_NAME) {
                return Err(format!("{trimmed} is a reserved context name"));
            }
            if let Some(other) = snapshot
                .contexts
                .iter()
                .find(|context| Some(context.id) != renaming && fold(&context.name) == folded)
            {
                return Err(format!("A context named \"{}\" already exists", other.name));
            }
            Ok(trimmed.to_string())
        };
        Ok(match self {
            Self::Switch(key) => match key.context_key() {
                ContextKey::Everything => ContextCommand::ShowEverything,
                ContextKey::Unsorted => {
                    ContextCommand::SwitchContext(ContextRef::Name(UNSORTED_NAME.into()))
                }
                ContextKey::Named(id) => {
                    ContextCommand::SwitchContext(ContextRef::Id(named(NamedKey { id: id.get() })?))
                }
            },
            Self::AddWindow { window, context } => ContextCommand::AddWindow {
                window: Some(window),
                context: ContextRef::Id(named(context)?),
            },
            Self::MoveWindow { window, context } => ContextCommand::MoveWindow {
                window: Some(window),
                context: ContextRef::Id(named(context)?),
            },
            Self::TogglePinned { window } => ContextCommand::TogglePinned { window: Some(window) },
            Self::Create { name: value, windows } => ContextCommand::CreateContextFromWindows {
                name: name(&value, None)?,
                windows,
            },
            Self::Edit {
                context,
                add,
                remove,
                remove_records,
            } => {
                let id = named(context)?;
                let context = ContextRef::Id(id);
                let summary = snapshot.get(id).expect("checked id");
                for item in &remove_records {
                    if !summary.members.get(item.record).is_some_and(|member| {
                        member.window.is_none()
                            && member.app == item.app
                            && member.title == item.title
                    }) {
                        return Err("A closed window changed while the switcher was open. Open the switcher again.".into());
                    }
                }
                ContextCommand::EditContext {
                    context,
                    add,
                    remove,
                    remove_records,
                }
            }
            Self::Rename { context, name: value } => {
                let id = named(context)?;
                let context = ContextRef::Id(id);
                ContextCommand::RenameContext {
                    context,
                    name: name(&value, Some(id))?,
                }
            }
            Self::SetNumber { context, number } => {
                if !(1..=9).contains(&number) {
                    return Err("Context number must be from 1 to 9".into());
                }
                ContextCommand::SetContextNumber {
                    context: ContextRef::Id(named(context)?),
                    number,
                }
            }
            Self::Delete(context) => ContextCommand::DeleteContext(ContextRef::Id(named(context)?)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::contexts_snapshot::{ContextSummary, MemberSummary};

    fn snapshot() -> ContextsSnapshot {
        ContextsSnapshot {
            enabled: true,
            contexts: vec![
                ContextSummary {
                    id: ContextId::from_raw(1),
                    name: "Café".into(),
                    number: Some(1),
                    last_used: 4,
                    apps: vec![],
                    windows: 0,
                    members: vec![MemberSummary {
                        record: 0,
                        app: "Mail".into(),
                        title: "Inbox".into(),
                        window: None,
                    }],
                },
                ContextSummary {
                    id: ContextId::from_raw(4),
                    name: "Client work".into(),
                    number: None,
                    last_used: 8,
                    apps: vec![],
                    windows: 0,
                    members: vec![],
                },
            ],
            ..ContextsSnapshot::off()
        }
    }

    fn check(json: &str) -> Result<ContextCommand, String> {
        serde_json::from_str::<SwitcherCommand>(json).unwrap().checked(&snapshot())
    }

    #[test]
    fn rank_uses_snapshot_names_and_mru_order() {
        let ranked = rank_snapshot("", &snapshot());
        assert_eq!(
            serde_json::to_value(ranked).unwrap(),
            serde_json::json!([
                {"key": {"id": 4}, "match": "empty_query"},
                {"key": {"id": 1}, "match": "empty_query"},
                {"key": "everything", "match": "empty_query"}
            ])
        );
        assert_eq!(
            serde_json::to_value(rank_snapshot("cafe", &snapshot())).unwrap(),
            serde_json::json!([{"key": {"id": 1}, "match": "exact"}])
        );
    }

    #[test]
    fn commands_keep_ids_and_carried_windows() {
        let wid = WindowId::new(812, 9123);
        let window = serde_json::to_string(&wid).unwrap();
        let add = format!(r#"{{"add_window":{{"window":{window},"context":{{"id":4}}}}}}"#);
        assert_eq!(
            check(&add),
            Ok(ContextCommand::AddWindow {
                window: Some(wid),
                context: ContextRef::Id(ContextId::from_raw(4)),
            })
        );
        assert_eq!(
            check(r#"{"switch":{"id":4}}"#),
            Ok(ContextCommand::SwitchContext(ContextRef::Id(
                ContextId::from_raw(4)
            )))
        );
        assert_eq!(
            check(r#"{"switch":"everything"}"#),
            Ok(ContextCommand::ShowEverything)
        );
        assert!(check(r#"{"switch":{"id":9}}"#).is_err());
        assert!(serde_json::from_str::<SwitcherCommand>(r#"{"switch":4}"#).is_err());
    }

    #[test]
    fn names_numbers_and_records_are_checked_against_snapshot() {
        assert!(check(r#"{"create":{"name":"  CAFÉ  ","windows":[]}}"#).is_err());
        assert!(check(r#"{"create":{"name":"Unsorted","windows":[]}}"#).is_err());
        assert!(check(r#"{"rename":{"context":{"id":4},"name":" "}}"#).is_err());
        assert!(check(r#"{"set_number":{"context":{"id":4},"number":0}}"#).is_err());
        let valid = r#"{"edit":{"context":{"id":1},"add":[],"remove":[],"remove_records":[{"record":0,"app":"Mail","title":"Inbox"}]}}"#;
        assert!(matches!(check(valid), Ok(ContextCommand::EditContext { .. })));
        let stale = valid.replace("Inbox", "Changed");
        assert!(check(&stale).is_err());
    }
}

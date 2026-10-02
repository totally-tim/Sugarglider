// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Contexts: named sets of windows that the user switches between.
//!
//! This module holds membership, the active context, and the pure decisions
//! about which windows to show. It does no I/O. Most-recently-used order is a
//! sequence number that increments on each switch or focus, not a timestamp.
//! The design is in `docs/specs/contexts.md`.

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::actor::app::{WindowId, pid_t};
use crate::collections::{HashMap, HashSet};
use crate::sys::screen::ScreenId;
use crate::sys::window_server::WindowServerId;

/// Identifies a named context. Ids are never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextId(u32);

impl ContextId {
    /// Reconstructs an id supplied by an external context command.
    pub fn from_raw(id: u32) -> Self {
        Self(id)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// A context the user can switch to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContextKey {
    /// Shows every window in the Space's normal layout.
    Everything,
    /// Shows the windows that belong to no named context.
    Unsorted,
    Named(ContextId),
}

/// Which screens a switch changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// A switch changes every screen.
    #[default]
    Global,
    /// A switch changes only the focused screen, and takes the target's
    /// members along from the other screens (R8, R9).
    PerScreen,
}

/// How a member record relates to a live window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecordLink {
    /// No live window. The record can match a window that appears.
    #[default]
    Empty,
    /// The record describes this open window.
    Live(WindowId),
    /// The window closed. The record waits to learn whether its app quit,
    /// and takes part in no matching until then.
    Pending(WindowId),
}

/// The stored description of a member window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberRecord {
    #[serde(default)]
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub app_name: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub window_server_id: Option<WindowServerId>,
    #[serde(skip)]
    pub link: RecordLink,
}

impl MemberRecord {
    fn for_window(window: &WindowDesc) -> Self {
        MemberRecord {
            bundle_id: window.bundle_id.clone(),
            app_name: window.app_name.clone(),
            title: window.title.clone(),
            window_server_id: window.window_server_id,
            link: RecordLink::Live(window.wid),
        }
    }

    /// The open window this record describes.
    pub fn window(&self) -> Option<WindowId> {
        match self.link {
            RecordLink::Live(wid) => Some(wid),
            RecordLink::Empty | RecordLink::Pending(_) => None,
        }
    }
}

/// A live window, as the actor describes it to the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowDesc {
    pub wid: WindowId,
    pub bundle_id: Option<String>,
    pub app_name: Option<String>,
    pub title: String,
    pub window_server_id: Option<WindowServerId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    pub id: ContextId,
    #[serde(default)]
    pub name: String,
    #[serde(default, deserialize_with = "saved_number")]
    pub number: Option<u8>,
    #[serde(default)]
    pub members: Vec<MemberRecord>,
    /// The value of the use sequence at the last switch to this context.
    #[serde(default)]
    pub last_used: u64,
}

impl Context {
    fn has_window(&self, wid: WindowId) -> bool {
        self.members.iter().any(|m| m.window() == Some(wid))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    #[error("A context name can't be empty")]
    EmptyName,
    #[error("\"{0}\" is a reserved name")]
    ReservedName(String),
    #[error("A context named \"{0}\" already exists")]
    NameTaken(String),
    #[error("Context numbers go from 1 to 9, not {0}")]
    NumberOutOfRange(u8),
    #[error("No such context")]
    NoSuchContext,
    #[error("No such member record")]
    NoSuchRecord,
    #[error("No context has the number {0}")]
    NoContextNumbered(u8),
    #[error("Give the name or the number of a context")]
    NoQuery,
    #[error("No context matches \"{0}\"")]
    NoMatch(String),
}

pub const EVERYTHING_NAME: &str = "Everything";
pub const UNSORTED_NAME: &str = "Unsorted";

/// What one screen shows in `per_screen` scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenState {
    pub active: ContextKey,
    /// The context the screen used before its active one (R18).
    pub previous: Option<ContextKey>,
}

impl Default for ScreenState {
    fn default() -> Self {
        ScreenState {
            active: ContextKey::Everything,
            previous: None,
        }
    }
}

/// The most empty member records that a context, or the pinned list, keeps.
/// An empty record's window is gone, and the record waits for a window to
/// match it.
pub const MAX_EMPTY_RECORDS: usize = 50;

/// The user's contexts, their members, and the active context.
///
/// Scope is global: one active context covers every screen. In `per_screen`
/// scope, which the reactor turns on with the config and applies through the
/// `switch_to_on` and `active_on` methods, each screen has its own active
/// context.
///
/// Serializes to the shape of `contexts.json`. Live windows, pending
/// states, the previous context, and focus order are not saved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "ContextsFile", try_from = "ContextsFile")]
pub struct Contexts {
    contexts: Vec<Context>,
    /// Windows that are members of every context, including later ones.
    pinned: Vec<MemberRecord>,
    next_id: u32,
    use_seq: u64,
    active: ContextKey,
    previous: Option<ContextKey>,
    /// The active context of each screen, in `per_screen` scope (R8).
    screens: HashMap<ScreenId, ScreenState>,
    unsorted_last_used: u64,
    everything_last_used: u64,
    focus_seq: u64,
    last_focus: HashMap<WindowId, u64>,
}

impl Default for Contexts {
    fn default() -> Self {
        Contexts {
            contexts: Vec::new(),
            pinned: Vec::new(),
            next_id: 1,
            use_seq: 0,
            active: ContextKey::Everything,
            previous: None,
            screens: HashMap::default(),
            unsorted_last_used: 0,
            everything_last_used: 0,
            focus_seq: 0,
            last_focus: HashMap::default(),
        }
    }
}

impl Contexts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contexts(&self) -> &[Context] {
        &self.contexts
    }

    pub fn pinned(&self) -> &[MemberRecord] {
        &self.pinned
    }

    pub fn get(&self, id: ContextId) -> Option<&Context> {
        self.contexts.iter().find(|c| c.id == id)
    }

    fn get_mut(&mut self, id: ContextId) -> Result<&mut Context, ContextError> {
        self.contexts.iter_mut().find(|c| c.id == id).ok_or(ContextError::NoSuchContext)
    }

    pub fn by_number(&self, number: u8) -> Option<&Context> {
        self.contexts.iter().find(|c| c.number == Some(number))
    }

    /// Finds a context by name, ignoring case and accents.
    pub fn by_name(&self, name: &str) -> Option<&Context> {
        let name = fold(name.trim());
        self.contexts.iter().find(|c| fold(&c.name) == name)
    }

    pub fn active(&self) -> ContextKey {
        self.active
    }

    /// The context a screen shows in `per_screen` scope. A screen with no
    /// entry shows Everything.
    pub fn active_on(&self, screen: ScreenId) -> ContextKey {
        self.screens.get(&screen).map_or(ContextKey::Everything, |state| state.active)
    }

    pub fn has_screen_actives(&self) -> bool {
        !self.screens.is_empty()
    }

    /// The context that was active before the current one.
    pub fn previous(&self) -> Option<ContextKey> {
        self.previous
    }

    /// The context a screen used before its active one (R18).
    pub fn previous_on(&self, screen: ScreenId) -> Option<ContextKey> {
        self.screens.get(&screen).and_then(|state| state.previous)
    }

    pub fn last_used(&self, key: ContextKey) -> u64 {
        match key {
            ContextKey::Everything => self.everything_last_used,
            ContextKey::Unsorted => self.unsorted_last_used,
            ContextKey::Named(id) => self.get(id).map_or(0, |c| c.last_used),
        }
    }

    /// Trims a name and checks that it isn't empty, reserved, or another
    /// context's name. Names are compared ignoring case and accents, as the
    /// switcher compares them.
    fn checked_name(
        &self,
        name: &str,
        renaming: Option<ContextId>,
    ) -> Result<String, ContextError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ContextError::EmptyName);
        }
        let folded = fold(name);
        if folded == fold(EVERYTHING_NAME) || folded == fold(UNSORTED_NAME) {
            return Err(ContextError::ReservedName(name.to_string()));
        }
        if let Some(other) = self.by_name(name)
            && Some(other.id) != renaming
        {
            return Err(ContextError::NameTaken(other.name.clone()));
        }
        Ok(name.to_string())
    }

    /// A name for a loaded context that follows R4: the trimmed name, with a
    /// number added when it is reserved or taken. An empty name becomes
    /// "Context" with a number.
    fn repaired_name(&self, name: &str) -> String {
        if let Ok(name) = self.checked_name(name, None) {
            return name;
        }
        let (base, first) = match name.trim() {
            "" => ("Context", 1),
            trimmed => (trimmed, 2),
        };
        (first..)
            .map(|n: u64| format!("{base} {n}"))
            .find(|candidate| self.checked_name(candidate, None).is_ok())
            .expect("fewer contexts than numbers")
    }

    /// Takes an id that no context has. Ids count up from `next_id`, so
    /// they aren't reused until they run out at `u32::MAX`. After that, the
    /// lowest free id is taken.
    fn take_id(&mut self) -> ContextId {
        if let Some(next) = self.next_id.checked_add(1) {
            let id = ContextId(self.next_id);
            self.next_id = next;
            return id;
        }
        (1..=u32::MAX)
            .map(ContextId)
            .find(|id| self.get(*id).is_none())
            .expect("fewer than u32::MAX contexts")
    }

    /// Takes the next use number. When the numbers run out, the uses are
    /// first numbered again from 1, in the same order.
    fn take_use(&mut self) -> u64 {
        if self.use_seq == u64::MAX {
            let mut used: Vec<u64> = self
                .contexts
                .iter()
                .map(|c| c.last_used)
                .chain([self.unsorted_last_used, self.everything_last_used])
                .filter(|n| *n > 0)
                .collect();
            used.sort_unstable();
            used.dedup();
            let renumber = |n: u64| {
                if n == 0 {
                    0
                } else {
                    used.partition_point(|u| *u < n) as u64 + 1
                }
            };
            for context in &mut self.contexts {
                context.last_used = renumber(context.last_used);
            }
            self.unsorted_last_used = renumber(self.unsorted_last_used);
            self.everything_last_used = renumber(self.everything_last_used);
            self.use_seq = used.len() as u64;
        }
        self.use_seq += 1;
        self.use_seq
    }

    /// Creates an empty context. It gets the lowest free number from 1 to 9,
    /// or none when all are taken.
    pub fn create(&mut self, name: &str) -> Result<ContextId, ContextError> {
        let name = self.checked_name(name, None)?;
        let number = (1..=9).find(|n| self.by_number(*n).is_none());
        let id = self.take_id();
        self.contexts.push(Context {
            id,
            name,
            number,
            members: Vec::new(),
            last_used: 0,
        });
        Ok(id)
    }

    pub fn rename(&mut self, id: ContextId, name: &str) -> Result<(), ContextError> {
        let name = self.checked_name(name, Some(id))?;
        self.get_mut(id)?.name = name;
        Ok(())
    }

    /// Gives a context a number, or removes its number. A context that
    /// already has the number loses it.
    pub fn set_number(&mut self, id: ContextId, number: Option<u8>) -> Result<(), ContextError> {
        if let Some(n) = number
            && !(1..=9).contains(&n)
        {
            return Err(ContextError::NumberOutOfRange(n));
        }
        self.get_mut(id)?;
        for context in &mut self.contexts {
            if context.id == id {
                context.number = number;
            } else if number.is_some() && context.number == number {
                context.number = None;
            }
        }
        Ok(())
    }

    /// Deletes a context. Its windows stay open; the ones that were only in
    /// this context become unsorted. If it was active, on any screen or in
    /// global scope, Unsorted becomes active so that those windows stay
    /// visible. That counts as a use of Unsorted, and the context before the
    /// deleted one stays the previous context unless it is Unsorted.
    pub fn delete(&mut self, id: ContextId) -> Result<Context, ContextError> {
        let idx = self
            .contexts
            .iter()
            .position(|c| c.id == id)
            .ok_or(ContextError::NoSuchContext)?;
        let context = self.contexts.remove(idx);
        let key = ContextKey::Named(id);
        let was_global = self.active == key;
        let mut screen_became_unsorted = false;
        for state in self.screens.values_mut() {
            if state.active == key {
                state.active = ContextKey::Unsorted;
                screen_became_unsorted = true;
            }
            if state.previous == Some(key) || state.previous == Some(state.active) {
                state.previous = None;
            }
        }
        if was_global {
            self.active = ContextKey::Unsorted;
        }
        if was_global || screen_became_unsorted {
            self.unsorted_last_used = self.take_use();
        }
        if self.previous == Some(key) || self.previous == Some(self.active) {
            self.previous = None;
        }
        Ok(context)
    }

    /// Adds a window to a context. Returns false if it was already a member.
    pub fn add_window(&mut self, id: ContextId, window: &WindowDesc) -> Result<bool, ContextError> {
        let context = self.get_mut(id)?;
        if context.has_window(window.wid) {
            return Ok(false);
        }
        context.members.push(MemberRecord::for_window(window));
        Ok(true)
    }

    /// Removes a window from a context. Returns false if it wasn't a member.
    pub fn remove_window(&mut self, id: ContextId, wid: WindowId) -> Result<bool, ContextError> {
        let context = self.get_mut(id)?;
        let len = context.members.len();
        context.members.retain(|m| m.window() != Some(wid));
        Ok(context.members.len() != len)
    }

    /// Removes a member record from a context or from the pinned list,
    /// whether or not it has a live window. `index` points into the slot's
    /// records. This is how the user removes the record of a window that is
    /// gone.
    pub fn remove_record(
        &mut self,
        slot: Slot,
        index: usize,
    ) -> Result<MemberRecord, ContextError> {
        let records = match slot {
            Slot::Pinned => &mut self.pinned,
            Slot::Context(id) => &mut self.get_mut(id)?.members,
        };
        if index >= records.len() {
            return Err(ContextError::NoSuchRecord);
        }
        Ok(records.remove(index))
    }

    /// Moves a window out of its screen's active context and into `target`.
    pub fn move_window(
        &mut self,
        active: ContextKey,
        target: ContextId,
        window: &WindowDesc,
    ) -> Result<(), ContextError> {
        self.get_mut(target)?;
        if let ContextKey::Named(active) = active
            && active != target
        {
            self.remove_window(active, window.wid)?;
        }
        self.add_window(target, window)?;
        Ok(())
    }

    pub fn is_pinned(&self, wid: WindowId) -> bool {
        self.pinned.iter().any(|m| m.window() == Some(wid))
    }

    /// Makes a window a member of every context. Its records in named
    /// contexts stay, so unpinning leaves it where it was. Returns false if
    /// it was already pinned.
    pub fn pin(&mut self, window: &WindowDesc) -> bool {
        if self.is_pinned(window.wid) {
            return false;
        }
        self.pinned.push(MemberRecord::for_window(window));
        true
    }

    /// Returns false if the window wasn't pinned.
    pub fn unpin(&mut self, wid: WindowId) -> bool {
        let len = self.pinned.len();
        self.pinned.retain(|m| m.window() != Some(wid));
        self.pinned.len() != len
    }

    /// Records that a context is used: it exists and its use number becomes
    /// the newest. Switching to the active context again is a use.
    fn note_use_of(&mut self, key: ContextKey) -> Result<(), ContextError> {
        if let ContextKey::Named(id) = key {
            self.get_mut(id)?;
        }
        let seq = self.take_use();
        match key {
            ContextKey::Everything => self.everything_last_used = seq,
            ContextKey::Unsorted => self.unsorted_last_used = seq,
            ContextKey::Named(id) => self.get_mut(id)?.last_used = seq,
        }
        Ok(())
    }

    /// Records a switch to `key`. Switching to the active context again
    /// counts as a use but leaves the previous context alone.
    pub fn switch_to(&mut self, key: ContextKey) -> Result<(), ContextError> {
        self.note_use_of(key)?;
        if self.active != key {
            self.previous = Some(self.active);
            self.active = key;
        }
        Ok(())
    }

    /// Records a switch on one screen (R8). Like [`Contexts::switch_to`],
    /// switching to the screen's active context again counts as a use.
    pub fn switch_to_on(&mut self, screen: ScreenId, key: ContextKey) -> Result<(), ContextError> {
        self.note_use_of(key)?;
        let state = self.screens.entry(screen).or_default();
        if state.active != key {
            state.previous = Some(state.active);
            state.active = key;
        }
        Ok(())
    }

    /// Gives each screen the context `key`, without counting a use. This is
    /// R11's change from `global` scope to `per_screen`: every screen starts
    /// on the context that was global.
    pub fn set_screen_actives(
        &mut self,
        screens: impl IntoIterator<Item = ScreenId>,
        key: ContextKey,
    ) {
        for screen in screens {
            self.screens.insert(
                screen,
                ScreenState {
                    active: key,
                    previous: self.previous,
                },
            );
        }
    }

    /// Forgets the per-screen contexts. This is R11's change from
    /// `per_screen` scope to `global`: the focused screen's context becomes
    /// the global one, and a later change back gives it to every screen.
    pub fn forget_screen_actives(&mut self) {
        self.screens.clear();
    }

    /// The named contexts that hold a record of this open window. Pinning is
    /// not included.
    pub fn contexts_of(&self, wid: WindowId) -> Vec<ContextId> {
        self.contexts.iter().filter(|c| c.has_window(wid)).map(|c| c.id).collect()
    }

    /// Whether an open window is in no named context and not pinned.
    pub fn is_unsorted(&self, wid: WindowId) -> bool {
        !self.is_pinned(wid) && !self.contexts.iter().any(|c| c.has_window(wid))
    }

    /// Whether an open window shows when `key` is active.
    pub fn is_member(&self, key: ContextKey, wid: WindowId) -> bool {
        match key {
            ContextKey::Everything => true,
            _ if self.is_pinned(wid) => true,
            ContextKey::Unsorted => self.is_unsorted(wid),
            ContextKey::Named(id) => self.get(id).is_some_and(|c| c.has_window(wid)),
        }
    }

    /// The context to switch to when the user focuses this window from
    /// outside the active context: the most recently used context that holds
    /// it, or Unsorted.
    #[cfg(test)]
    pub fn focus_target(&self, wid: WindowId) -> ContextKey {
        self.focus_target_on(self.active, wid)
    }

    /// The context to switch to when the user focuses this window from
    /// outside `active`: `active` when it holds the window, else the most
    /// recently used context that holds it, or Unsorted. In `per_screen`
    /// scope each screen has its own active context (R8, R26).
    pub fn focus_target_on(&self, active: ContextKey, wid: WindowId) -> ContextKey {
        if self.is_member(active, wid) {
            return active;
        }
        self.contexts
            .iter()
            .filter(|c| c.has_window(wid))
            .max_by_key(|c| c.last_used)
            .map_or(ContextKey::Unsorted, |c| ContextKey::Named(c.id))
    }

    /// Records that a window took focus.
    pub fn window_focused(&mut self, wid: WindowId) {
        self.focus_seq += 1;
        self.last_focus.insert(wid, self.focus_seq);
    }

    /// When a window last took focus, as a sequence number. Larger is later.
    pub fn last_focus(&self, wid: WindowId) -> Option<u64> {
        self.last_focus.get(&wid).copied()
    }
}

/// Lowercases text and removes accents, for comparing names and titles.
///
/// Folds the letters with marks, and the ligatures, of the Latin-1
/// Supplement, Latin Extended-A, Latin Extended-B, and Latin Extended
/// Additional blocks to ASCII, and drops combining diacritical marks.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if ('\u{0300}'..='\u{036F}').contains(&c) {
            continue;
        }
        match fold_char(c) {
            Some(folded) => out.push_str(folded),
            None => out.extend(c.to_lowercase()),
        }
    }
    out
}

fn fold_char(c: char) -> Option<&'static str> {
    Some(match c {
        'À'..='Å' | 'à'..='å' | '\u{0100}'..='\u{0105}' => "a",
        'Æ' | 'æ' => "ae",
        'Ç' | 'ç' | '\u{0106}'..='\u{010D}' => "c",
        'Ð' | 'ð' | '\u{010E}'..='\u{0111}' => "d",
        'È'..='Ë' | 'è'..='ë' | '\u{0112}'..='\u{011B}' => "e",
        '\u{011C}'..='\u{0123}' => "g",
        '\u{0124}'..='\u{0127}' => "h",
        'Ì'..='Ï' | 'ì'..='ï' | '\u{0128}'..='\u{0131}' => "i",
        '\u{0132}' | '\u{0133}' => "ij",
        '\u{0134}' | '\u{0135}' => "j",
        '\u{0136}'..='\u{0138}' => "k",
        '\u{0139}'..='\u{0142}' => "l",
        'Ñ' | 'ñ' | '\u{0143}'..='\u{014B}' => "n",
        'Ò'..='Ö' | 'Ø' | 'ò'..='ö' | 'ø' | '\u{014C}'..='\u{0151}' => "o",
        '\u{0152}' | '\u{0153}' => "oe",
        '\u{0154}'..='\u{0159}' => "r",
        'ß' => "ss",
        '\u{015A}'..='\u{0161}' | '\u{017F}' => "s",
        '\u{0162}'..='\u{0167}' => "t",
        'Þ' | 'þ' => "th",
        'Ù'..='Ü' | 'ù'..='ü' | '\u{0168}'..='\u{0173}' => "u",
        '\u{0174}' | '\u{0175}' => "w",
        'Ý' | 'ý' | 'ÿ' | '\u{0176}'..='\u{0178}' => "y",
        '\u{0179}'..='\u{017E}' => "z",
        '\u{0180}'..='\u{024F}' => return fold_latin_extended_b(c),
        '\u{1E00}'..='\u{1EFF}' => return fold_latin_extended_additional(c),
        _ => return None,
    })
}

/// Folds a letter of the Latin Extended-B block. Letters that aren't a
/// Latin letter with a mark or a digraph, such as schwa, ezh, and wynn,
/// are not folded.
fn fold_latin_extended_b(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{01CD}' | '\u{01CE}' | '\u{01DE}'..='\u{01E1}' | '\u{01FA}' | '\u{01FB}' => "a",
        '\u{0200}'..='\u{0203}' | '\u{0226}' | '\u{0227}' | '\u{023A}' => "a",
        '\u{01E2}' | '\u{01E3}' | '\u{01FC}' | '\u{01FD}' => "ae",
        '\u{0180}'..='\u{0183}' | '\u{0243}' => "b",
        '\u{0187}' | '\u{0188}' | '\u{023B}' | '\u{023C}' => "c",
        '\u{0189}'..='\u{018C}' | '\u{0221}' => "d",
        '\u{0238}' => "db",
        '\u{01C4}'..='\u{01C6}' | '\u{01F1}'..='\u{01F3}' => "dz",
        '\u{0204}'..='\u{0207}' | '\u{0228}' | '\u{0229}' | '\u{0246}' | '\u{0247}' => "e",
        '\u{0191}' | '\u{0192}' => "f",
        '\u{0193}' | '\u{01E4}'..='\u{01E7}' | '\u{01F4}' | '\u{01F5}' => "g",
        '\u{021E}' | '\u{021F}' => "h",
        '\u{0195}' | '\u{01F6}' => "hv",
        '\u{0197}' | '\u{01CF}' | '\u{01D0}' | '\u{0208}'..='\u{020B}' => "i",
        '\u{01F0}' | '\u{0237}' | '\u{0248}' | '\u{0249}' => "j",
        '\u{0198}' | '\u{0199}' | '\u{01E8}' | '\u{01E9}' => "k",
        '\u{019A}' | '\u{0234}' | '\u{023D}' => "l",
        '\u{01C7}'..='\u{01C9}' => "lj",
        '\u{019D}' | '\u{019E}' | '\u{01F8}' | '\u{01F9}' | '\u{0220}' | '\u{0235}' => "n",
        '\u{01CA}'..='\u{01CC}' => "nj",
        '\u{019F}'..='\u{01A1}' | '\u{01D1}' | '\u{01D2}' | '\u{01EA}'..='\u{01ED}' => "o",
        '\u{01FE}' | '\u{01FF}' | '\u{020C}'..='\u{020F}' | '\u{022A}'..='\u{0231}' => "o",
        '\u{01A2}' | '\u{01A3}' => "oi",
        '\u{0222}' | '\u{0223}' => "ou",
        '\u{01A4}' | '\u{01A5}' => "p",
        '\u{024A}' | '\u{024B}' => "q",
        '\u{0239}' => "qp",
        '\u{0210}'..='\u{0213}' | '\u{024C}' | '\u{024D}' => "r",
        '\u{0218}' | '\u{0219}' | '\u{023F}' => "s",
        '\u{01AB}'..='\u{01AE}' | '\u{021A}' | '\u{021B}' | '\u{0236}' | '\u{023E}' => "t",
        '\u{01AF}' | '\u{01B0}' | '\u{01D3}'..='\u{01DC}' | '\u{0214}'..='\u{0217}' => "u",
        '\u{0244}' => "u",
        '\u{01B2}' => "v",
        '\u{01B3}' | '\u{01B4}' | '\u{0232}' | '\u{0233}' | '\u{024E}' | '\u{024F}' => "y",
        '\u{01B5}' | '\u{01B6}' | '\u{0224}' | '\u{0225}' | '\u{0240}' => "z",
        _ => return None,
    })
}

/// Folds a letter of the Latin Extended Additional block, which holds most
/// Vietnamese letters. The letter delta is not folded.
fn fold_latin_extended_additional(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{1E00}' | '\u{1E01}' | '\u{1E9A}' | '\u{1EA0}'..='\u{1EB7}' => "a",
        '\u{1E02}'..='\u{1E07}' => "b",
        '\u{1E08}' | '\u{1E09}' => "c",
        '\u{1E0A}'..='\u{1E13}' => "d",
        '\u{1E14}'..='\u{1E1D}' | '\u{1EB8}'..='\u{1EC7}' => "e",
        '\u{1E1E}' | '\u{1E1F}' => "f",
        '\u{1E20}' | '\u{1E21}' => "g",
        '\u{1E22}'..='\u{1E2B}' | '\u{1E96}' => "h",
        '\u{1E2C}'..='\u{1E2F}' | '\u{1EC8}'..='\u{1ECB}' => "i",
        '\u{1E30}'..='\u{1E35}' => "k",
        '\u{1E36}'..='\u{1E3D}' => "l",
        '\u{1EFA}' | '\u{1EFB}' => "ll",
        '\u{1E3E}'..='\u{1E43}' => "m",
        '\u{1E44}'..='\u{1E4B}' => "n",
        '\u{1E4C}'..='\u{1E53}' | '\u{1ECC}'..='\u{1EE3}' => "o",
        '\u{1E54}'..='\u{1E57}' => "p",
        '\u{1E58}'..='\u{1E5F}' => "r",
        '\u{1E60}'..='\u{1E69}' | '\u{1E9B}'..='\u{1E9D}' => "s",
        '\u{1E9E}' => "ss",
        '\u{1E6A}'..='\u{1E71}' | '\u{1E97}' => "t",
        '\u{1E72}'..='\u{1E7B}' | '\u{1EE4}'..='\u{1EF1}' => "u",
        '\u{1E7C}'..='\u{1E7F}' | '\u{1EFC}' | '\u{1EFD}' => "v",
        '\u{1E80}'..='\u{1E89}' | '\u{1E98}' => "w",
        '\u{1E8A}'..='\u{1E8D}' => "x",
        '\u{1E8E}' | '\u{1E8F}' | '\u{1E99}' | '\u{1EF2}'..='\u{1EF9}' => "y",
        '\u{1EFE}' | '\u{1EFF}' => "y",
        '\u{1E90}'..='\u{1E95}' => "z",
        _ => return None,
    })
}

/// How well a query matches a context name, from weakest to strongest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameMatch {
    /// The query is empty, so every entry is listed.
    EmptyQuery,
    /// The query's letters, at least 2 of them, appear in the name in order.
    LettersInOrder,
    /// Each word of a query of two or more words starts a word of the name,
    /// in any order.
    AllWordPrefixes,
    /// The query, at least 2 characters long, starts the name's initials.
    Initials,
    /// The query starts a word of the name.
    WordPrefix,
    /// The query starts the name.
    NamePrefix,
    Exact,
}

/// Ranks the switcher's entries for a query, best first.
///
/// The entries are the named contexts, Unsorted when it has windows, and
/// Everything. Entries that don't match are left out, and a query without
/// letters or digits matches none. Ties go to the most recently used entry.
pub fn rank(
    query: &str,
    contexts: &Contexts,
    unsorted_has_windows: bool,
) -> Vec<(ContextKey, NameMatch)> {
    let unsorted = unsorted_has_windows.then(|| {
        (
            ContextKey::Unsorted,
            UNSORTED_NAME.to_string(),
            contexts.last_used(ContextKey::Unsorted),
        )
    });
    let entries = contexts
        .contexts
        .iter()
        .map(|c| (ContextKey::Named(c.id), c.name.clone(), c.last_used))
        .chain(unsorted)
        .chain([(
            ContextKey::Everything,
            EVERYTHING_NAME.to_string(),
            contexts.last_used(ContextKey::Everything),
        )]);
    rank_entries(query, entries)
}

/// Ranks `(key, name, last_used)` entries for a query, best first, as
/// [`rank`] does. A caller that has no `Contexts`, such as the switcher
/// bridge reading the published snapshot, builds the entries itself.
///
/// Ties go to the most recently used entry, and entries that are still tied
/// keep the order they came in.
pub fn rank_entries(
    query: &str,
    entries: impl IntoIterator<Item = (ContextKey, String, u64)>,
) -> Vec<(ContextKey, NameMatch)> {
    let query = fold(query.trim());
    let mut ranked: Vec<(ContextKey, NameMatch, u64)> = entries
        .into_iter()
        .filter_map(|(key, name, last_used)| {
            match_name(&query, &fold(&name)).map(|found| (key, found, last_used))
        })
        .collect();
    ranked.sort_by(|(_, a, a_used), (_, b, b_used)| b.cmp(a).then_with(|| b_used.cmp(a_used)));
    ranked.into_iter().map(|(key, found, _)| (key, found)).collect()
}

/// Names a context in a command: by its number, its id, or its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Query<'a> {
    Number(u8),
    Id(ContextId),
    Name(&'a str),
}

/// The entry that a command's query names.
///
/// A number or an id names the context that has it. A name names the
/// context with that name, ignoring case and accents. Everything and
/// Unsorted match only their exact names, and Unsorted only while it is
/// listed: `lists_unsorted` is true and a context exists (R28, R29). While
/// it isn't, its exact name names nothing, not a context whose name starts
/// with it. Any other name takes the best match of the switcher's ranking
/// among the named contexts.
pub fn resolve(
    query: Query<'_>,
    contexts: &Contexts,
    lists_unsorted: bool,
) -> Result<ContextKey, ContextError> {
    match query {
        Query::Number(number) => contexts
            .by_number(number)
            .map(|context| ContextKey::Named(context.id))
            .ok_or(ContextError::NoContextNumbered(number)),
        Query::Id(id) => contexts
            .get(id)
            .map(|context| ContextKey::Named(context.id))
            .ok_or(ContextError::NoSuchContext),
        Query::Name(name) if name.trim().is_empty() => Err(ContextError::NoQuery),
        Query::Name(name) => {
            let lists_unsorted = lists_unsorted && !contexts.contexts.is_empty();
            if !lists_unsorted && fold(name.trim()) == fold(UNSORTED_NAME) {
                return Err(ContextError::NoMatch(name.trim().to_string()));
            }
            rank(name, contexts, lists_unsorted)
                .into_iter()
                .find(|&(key, found)| {
                    matches!(key, ContextKey::Named(_)) || found == NameMatch::Exact
                })
                .map(|(key, _)| key)
                .ok_or_else(|| ContextError::NoMatch(name.trim().to_string()))
        }
    }
}

/// Matches a folded query against a folded name.
fn match_name(query: &str, name: &str) -> Option<NameMatch> {
    if query.is_empty() {
        return Some(NameMatch::EmptyQuery);
    }
    if !query.chars().any(char::is_alphanumeric) {
        return None;
    }
    if name == query {
        return Some(NameMatch::Exact);
    }
    if name.starts_with(query) {
        return Some(NameMatch::NamePrefix);
    }
    if word_starts(name).any(|start| name[start..].starts_with(query)) {
        return Some(NameMatch::WordPrefix);
    }
    let name_words: Vec<&str> = words(name).collect();
    let initials: String = name_words.iter().filter_map(|w| w.chars().next()).collect();
    let letters: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).collect();
    if letters.len() >= 2 && !query.contains(char::is_whitespace) && initials.starts_with(query) {
        return Some(NameMatch::Initials);
    }
    let query_words: Vec<&str> = words(query).collect();
    if query_words.len() >= 2
        && query_words.iter().all(|q| name_words.iter().any(|w| w.starts_with(q)))
    {
        return Some(NameMatch::AllWordPrefixes);
    }
    let mut name_chars = name.chars();
    if letters.len() >= 2 && letters.iter().all(|q| name_chars.any(|c| c == *q)) {
        return Some(NameMatch::LettersInOrder);
    }
    None
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty())
}

/// Byte offsets at which a word starts.
fn word_starts(text: &str) -> impl Iterator<Item = usize> {
    let mut previous_alphanumeric = false;
    text.char_indices().filter_map(move |(i, c)| {
        let starts = c.is_alphanumeric() && !previous_alphanumeric;
        previous_alphanumeric = c.is_alphanumeric();
        starts.then_some(i)
    })
}

/// When window matching runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchPass {
    /// Windows appeared, or the reactor found them at launch. Steps 1 to 3
    /// run.
    Arrival,
    /// A switch to `target` is in progress. Steps 1 to 3 run for every
    /// record. Step 4 runs only for the records of `target`, and not at all
    /// when `target` is Everything or Unsorted.
    Switch { target: ContextKey },
}

/// The step at which a window matched a member record, in the order the
/// steps run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchStep {
    /// Same window server id.
    WindowServerId,
    /// Same app and exactly the same title.
    ExactTitle,
    /// Same app and a similar title.
    SimilarTitle,
    /// Same app.
    SameApp,
}

/// Where a member record lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    Context(ContextId),
    Pinned,
}

/// A member record that a window matched. `index` points into the slot's
/// records and is valid until the next change to the contexts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordMatch {
    pub slot: Slot,
    pub index: usize,
    pub step: MatchStep,
}

/// Finds the empty member records that windows match.
///
/// Each step runs for every window before the next step starts, so a weak
/// match never takes a record that another window matches at an earlier
/// step. Within a step, each record takes the first window in `windows` that
/// it accepts. A record takes at most one window, and a window takes at most
/// one record from each context and from the pinned list.
///
/// Every step needs the same app. Steps 2 and 3 need a title that isn't
/// blank. Steps 3 and 4 take only windows that are in no context and not
/// pinned, and that matched nothing at an earlier step, so they never take a
/// window that belongs to another context. Step 4 runs only during a switch,
/// for the target's records. Pending records match nothing. Each window must
/// appear once in `windows`.
///
/// Returns the matches of each window, in the order of `windows`. A window's
/// matches are in step order, and within a step in the order of the
/// contexts, with the pinned list last.
pub fn match_windows(
    windows: &[WindowDesc],
    contexts: &Contexts,
    pass: MatchPass,
) -> Vec<Vec<RecordMatch>> {
    let slots: Vec<(Slot, &[MemberRecord])> = contexts
        .contexts
        .iter()
        .map(|c| (Slot::Context(c.id), c.members.as_slice()))
        .chain(std::iter::once((Slot::Pinned, contexts.pinned.as_slice())))
        .collect();
    let mut matches: Vec<Vec<RecordMatch>> = vec![Vec::new(); windows.len()];
    let mut taken: HashSet<(Slot, usize)> = HashSet::default();
    for step in [
        MatchStep::WindowServerId,
        MatchStep::ExactTitle,
        MatchStep::SimilarTitle,
        MatchStep::SameApp,
    ] {
        let open: Vec<bool> = windows
            .iter()
            .zip(&matches)
            .map(|(window, found)| match step {
                MatchStep::WindowServerId | MatchStep::ExactTitle => true,
                MatchStep::SimilarTitle | MatchStep::SameApp => {
                    found.is_empty() && contexts.is_unsorted(window.wid)
                }
            })
            .collect();
        for &(slot, records) in &slots {
            if step == MatchStep::SameApp
                && !matches!(pass, MatchPass::Switch { target: ContextKey::Named(id) }
                    if slot == Slot::Context(id))
            {
                continue;
            }
            for (index, record) in records.iter().enumerate() {
                if record.link != RecordLink::Empty || taken.contains(&(slot, index)) {
                    continue;
                }
                let found = (0..windows.len()).find(|&i| {
                    open[i]
                        && !matches[i].iter().any(|m| m.slot == slot)
                        && !records.iter().any(|m| m.window() == Some(windows[i].wid))
                        && accepts(step, record, &windows[i])
                });
                if let Some(i) = found {
                    matches[i].push(RecordMatch { slot, index, step });
                    taken.insert((slot, index));
                }
            }
        }
    }
    matches
}

/// [`match_windows`] for one window.
#[cfg(test)]
pub fn match_window(window: &WindowDesc, contexts: &Contexts, pass: MatchPass) -> Vec<RecordMatch> {
    match_windows(std::slice::from_ref(window), contexts, pass)
        .pop()
        .unwrap_or_default()
}

/// Whether a member record accepts a window at a step.
fn accepts(step: MatchStep, record: &MemberRecord, window: &WindowDesc) -> bool {
    same_app(record, window)
        && match step {
            MatchStep::WindowServerId => {
                window.window_server_id.is_some()
                    && record.window_server_id == window.window_server_id
            }
            MatchStep::ExactTitle => {
                !record.title.trim().is_empty() && record.title == window.title
            }
            MatchStep::SimilarTitle => {
                !record.title.trim().is_empty()
                    && !window.title.trim().is_empty()
                    && similar_titles(&record.title, &window.title)
            }
            MatchStep::SameApp => true,
        }
}

/// Compares bundle ids when both are known, and app names otherwise.
fn same_app(record: &MemberRecord, window: &WindowDesc) -> bool {
    match (&record.bundle_id, &window.bundle_id) {
        (Some(a), Some(b)) => a == b,
        _ => matches!((&record.app_name, &window.app_name), (Some(a), Some(b)) if a == b),
    }
}

/// Whether two window titles are similar: after folding, both have at least
/// 4 characters, and one contains the other or they share a prefix of at
/// least min(12, two-thirds of the shorter title), rounded down.
pub fn similar_titles(a: &str, b: &str) -> bool {
    let a = fold(a);
    let b = fold(b);
    let a_len = a.chars().count();
    let b_len = b.chars().count();
    if a_len < 4 || b_len < 4 {
        return false;
    }
    if a.contains(&b) || b.contains(&a) {
        return true;
    }
    let shorter = a_len.min(b_len);
    let prefix = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    prefix >= (shorter * 2 / 3).min(12)
}

/// What happened to a window that appeared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// It matched member records and rejoined the contexts that hold them.
    Rejoined(Vec<RecordMatch>),
    /// It matched nothing and joined the active context of its screen.
    Joined(ContextId),
    /// It matched nothing, and its screen shows Everything or Unsorted.
    Unsorted,
    /// It is already a member of a context or pinned, so nothing changed.
    AlreadyMember,
}

impl Contexts {
    fn slot_records_mut(&mut self, slot: Slot) -> &mut Vec<MemberRecord> {
        match slot {
            Slot::Pinned => &mut self.pinned,
            Slot::Context(id) => {
                &mut self
                    .contexts
                    .iter_mut()
                    .find(|c| c.id == id)
                    .expect("matched context exists")
                    .members
            }
        }
    }

    fn records_mut(&mut self) -> impl Iterator<Item = &mut MemberRecord> {
        self.contexts
            .iter_mut()
            .flat_map(|c| c.members.iter_mut())
            .chain(self.pinned.iter_mut())
    }

    /// Matches windows against the member records together (see
    /// [`match_windows`]) and binds each window to the records it matches.
    /// The records take the window's current details. Returns the matches of
    /// each window, in the order of `windows`.
    ///
    /// Windows that the reactor finds before `StartupComplete` were open
    /// before Sugarglider started. They come here with
    /// [`MatchPass::Arrival`], not to [`Contexts::windows_appeared`], so the
    /// ones that match nothing stay unsorted.
    pub fn rejoin_all(&mut self, windows: &[WindowDesc], pass: MatchPass) -> Vec<Vec<RecordMatch>> {
        let matches = match_windows(windows, self, pass);
        for (window, found) in windows.iter().zip(&matches) {
            for m in found {
                self.slot_records_mut(m.slot)[m.index] = MemberRecord::for_window(window);
            }
        }
        matches
    }

    /// [`Contexts::rejoin_all`] for one window.
    #[cfg(test)]
    pub fn rejoin(&mut self, window: &WindowDesc, pass: MatchPass) -> Vec<RecordMatch> {
        self.rejoin_all(std::slice::from_ref(window), pass).pop().unwrap_or_default()
    }

    /// Decides the membership of windows that appeared on a screen that
    /// shows `screen_active`, and returns what happened to each, in the
    /// order of `windows`.
    ///
    /// The windows are matched together (see [`match_windows`]). A window
    /// that matches member records rejoins their contexts and does not join
    /// the active context. Otherwise it joins the active context, or stays
    /// unsorted under Everything and Unsorted.
    ///
    /// Pass only windows that the reactor sees for the first time after
    /// `StartupComplete`. The reactor keeps the set of windows it has seen,
    /// because this can't tell an unsorted window that comes back, for
    /// example from being minimized, from a new one: both would join the
    /// active context. Windows found before `StartupComplete` go to
    /// [`Contexts::rejoin_all`].
    pub fn windows_appeared(
        &mut self,
        windows: &[WindowDesc],
        screen_active: ContextKey,
    ) -> Vec<Arrival> {
        let unsorted: Vec<bool> = windows.iter().map(|w| self.is_unsorted(w.wid)).collect();
        let new: Vec<WindowDesc> = windows
            .iter()
            .zip(&unsorted)
            .filter(|(_, unsorted)| **unsorted)
            .map(|(window, _)| window.clone())
            .collect();
        let mut matches = self.rejoin_all(&new, MatchPass::Arrival).into_iter();
        let mut arrivals = Vec::with_capacity(windows.len());
        for (window, unsorted) in windows.iter().zip(unsorted) {
            let arrival = if !unsorted {
                Arrival::AlreadyMember
            } else {
                let found = matches.next().unwrap_or_default();
                if !found.is_empty() {
                    Arrival::Rejoined(found)
                } else if let ContextKey::Named(id) = screen_active
                    && let Ok(context) = self.get_mut(id)
                {
                    context.members.push(MemberRecord::for_window(window));
                    Arrival::Joined(id)
                } else {
                    Arrival::Unsorted
                }
            };
            arrivals.push(arrival);
        }
        arrivals
    }

    /// [`Contexts::windows_appeared`] for one window.
    #[cfg(test)]
    pub fn window_appeared(&mut self, window: &WindowDesc, screen_active: ContextKey) -> Arrival {
        self.windows_appeared(std::slice::from_ref(window), screen_active)
            .pop()
            .expect("one arrival per window")
    }

    /// Forgets the window server ids of the records that have no live
    /// window. Window server ids are valid within one login session, so the
    /// actor calls this when it loads `contexts.json` in a new one.
    pub fn forget_window_server_ids(&mut self) {
        for record in self.records_mut() {
            if record.window().is_none() {
                record.window_server_id = None;
            }
        }
    }

    /// Updates the records of an open window with its new title.
    pub fn title_changed(&mut self, wid: WindowId, title: &str) {
        for record in self.records_mut() {
            if record.window() == Some(wid) {
                record.title = title.to_string();
            }
        }
    }

    /// Marks the records of a closed window as pending until its app shows
    /// whether it quit.
    pub fn window_closed(&mut self, wid: WindowId) {
        self.last_focus.remove(&wid);
        for record in self.records_mut() {
            if record.link == RecordLink::Live(wid) {
                record.link = RecordLink::Pending(wid);
            }
        }
    }

    /// The app quit. All of its records stay, open or pending, and wait for
    /// its windows to appear again.
    ///
    /// A context, and the pinned list, keeps at most [`MAX_EMPTY_RECORDS`]
    /// empty records. When the app's records take it over that, the oldest
    /// empty records go: first the ones that were empty already, then the
    /// app's own, each in list order, which is the order the records were
    /// added. Contexts and the pinned list without records of the app are
    /// left alone. Loading `contexts.json` applies no limit, because every
    /// record is empty after a restart.
    pub fn app_terminated(&mut self, pid: pid_t) {
        self.last_focus.retain(|wid, _| wid.pid != pid);
        let lists = self
            .contexts
            .iter_mut()
            .map(|c| &mut c.members)
            .chain(std::iter::once(&mut self.pinned));
        for records in lists {
            let emptied: Vec<bool> = records
                .iter()
                .map(|m| match m.link {
                    RecordLink::Live(wid) | RecordLink::Pending(wid) => wid.pid == pid,
                    RecordLink::Empty => false,
                })
                .collect();
            if !emptied.contains(&true) {
                continue;
            }
            for (record, emptied) in records.iter_mut().zip(&emptied) {
                if *emptied {
                    record.link = RecordLink::Empty;
                }
            }
            drop_oldest_empty_records(records, &emptied);
        }
    }

    /// The app is still running, so its closed windows were closed for good.
    /// Deletes their pending records.
    pub fn app_still_running(&mut self, pid: pid_t) {
        let still_running =
            |m: &MemberRecord| !matches!(m.link, RecordLink::Pending(wid) if wid.pid == pid);
        for context in &mut self.contexts {
            context.members.retain(still_running);
        }
        self.pinned.retain(still_running);
    }
}

/// Drops empty records until at most [`MAX_EMPTY_RECORDS`] are left, first
/// the ones not marked in `newest`, then the marked ones, each from the
/// front of the list.
fn drop_oldest_empty_records(records: &mut Vec<MemberRecord>, newest: &[bool]) {
    let empty = records.iter().filter(|m| m.link == RecordLink::Empty).count();
    let excess = empty.saturating_sub(MAX_EMPTY_RECORDS);
    if excess == 0 {
        return;
    }
    let mut dropped: Vec<usize> = (0..records.len())
        .filter(|&i| !newest[i])
        .chain((0..records.len()).filter(|&i| newest[i]))
        .filter(|&i| records[i].link == RecordLink::Empty)
        .take(excess)
        .collect();
    dropped.sort_unstable();
    let mut index = 0;
    records.retain(|_| {
        let keep = dropped.binary_search(&index).is_err();
        index += 1;
        keep
    });
}

/// The windows of a visible screen, for planning a switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchScreen {
    /// The screen's active context after the switch.
    pub active: ContextKey,
    /// The windows on the screen's visible Space, and the windows that are
    /// only on its other Spaces, with `invisible` set. Every parked window
    /// must be listed, so that showing Everything puts each one back.
    pub windows: Vec<SwitchWindow>,
}

/// A window on a visible screen, for planning a switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchWindow {
    pub wid: WindowId,
    /// The named contexts that hold the window.
    pub contexts: Vec<ContextId>,
    pub pinned: bool,
    /// The layout manager doesn't track the window.
    pub untracked: bool,
    /// The window isn't in the reactor's set of visible windows: it is
    /// minimized, the user hid its app, or it is only on Spaces nobody can
    /// see.
    pub invisible: bool,
    pub parked: bool,
    /// The window belongs to Sugarglider.
    pub own: bool,
    /// When the window last took focus, as a sequence number.
    pub last_focus: Option<u64>,
}

impl SwitchWindow {
    fn shows_under(&self, key: ContextKey) -> bool {
        match key {
            ContextKey::Everything => true,
            _ if self.pinned => true,
            ContextKey::Unsorted => self.contexts.is_empty(),
            ContextKey::Named(id) => self.contexts.contains(&id),
        }
    }

    /// Whether a switch to `key` takes the window along to the screen that
    /// switched (R8, R9). A pinned window shows on every screen but stays
    /// where it is, and Everything changes no screen's members.
    fn comes_with(&self, key: ContextKey) -> bool {
        if self.pinned {
            return false;
        }
        match key {
            ContextKey::Everything => false,
            ContextKey::Unsorted => self.contexts.is_empty(),
            ContextKey::Named(id) => self.contexts.contains(&id),
        }
    }

    /// Whether the user can see and use the window, and Sugarglider may move it.
    fn in_play(&self) -> bool {
        !(self.own || self.untracked || self.invisible)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchInput {
    pub screens: Vec<SwitchScreen>,
    /// `Some(i)` when the switch changes only the screen at `i`, as
    /// `per_screen` scope does: the members of its context move there from
    /// the other screens (R8, R9). `None` changes every screen, and windows
    /// stay on the screen they're on (R7).
    pub only: Option<usize>,
}

/// A window a switch moves to another screen, and that screen's place in
/// [`SwitchInput::screens`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwitchMove {
    pub wid: WindowId,
    pub screen: usize,
}

/// What a switch does to windows. In `global` scope windows keep the screen
/// they're on; a `per_screen` switch moves members to the screen it changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwitchPlan {
    /// Windows to park, which must be written to the journal first.
    pub park: Vec<WindowId>,
    /// Parked windows to put back.
    pub unpark: Vec<WindowId>,
    /// Windows to move to another screen (R8, R9). A parked one is also in
    /// `unpark`, because it must show at its destination.
    pub moves: Vec<SwitchMove>,
    /// The window to focus with a quiet raise.
    pub focus: Option<WindowId>,
}

/// Plans a switch: which windows to park, which to put back, which to move
/// to another screen, and which to focus.
///
/// A window must show when it is a member of its screen's active context;
/// under Everything every window must show. Windows that must show and are
/// parked are put back to their journal frames, whatever their state. Under
/// Everything that is every parked window in the input, which is how
/// showing Everything, quitting, and turning Sugarglider off restore windows
/// (R27, R32, R33). The others are parked, except Sugarglider's own windows,
/// untracked windows, and windows that aren't in the reactor's set of visible
/// windows. Windows that are parked already stay parked.
///
/// With [`SwitchInput::only`] set, the windows of the target context that are
/// on the other screens move to the screen that switched, and that screen's
/// focus goes to the most recently focused window that shows there or moves
/// there. Otherwise the focus goes to the most recently focused window that
/// shows and that the user can see, on any screen.
pub fn plan_switch(input: &SwitchInput) -> SwitchPlan {
    let mut plan = SwitchPlan::default();
    let mut focus: Option<&SwitchWindow> = None;
    let taking = input.only.and_then(|idx| input.screens.get(idx).map(|taking| (idx, taking)));
    for (idx, screen) in input.screens.iter().enumerate() {
        for window in &screen.windows {
            let moves_here = taking.is_some_and(|(taker, taker_screen)| {
                idx != taker && window.in_play() && window.comes_with(taker_screen.active)
            });
            if taking.is_some_and(|(taker, _)| idx != taker) && !moves_here {
                continue;
            }
            if moves_here {
                let taker = taking.expect("checked above").0;
                if window.parked {
                    plan.unpark.push(window.wid);
                }
                plan.moves.push(SwitchMove { wid: window.wid, screen: taker });
            } else if window.shows_under(screen.active) {
                if window.parked {
                    plan.unpark.push(window.wid);
                }
            } else if !window.parked && window.in_play() {
                plan.park.push(window.wid);
                continue;
            }
            let shows_on_the_taker = taking.is_none_or(|(taker, _)| idx == taker);
            if window.in_play()
                && (moves_here || shows_on_the_taker && window.shows_under(screen.active))
                && focus.is_none_or(|f| window.last_focus > f.last_focus)
            {
                focus = Some(window);
            }
        }
    }
    plan.focus = focus.map(|w| w.wid);
    plan
}

impl Contexts {
    /// Describes a window's membership for [`plan_switch`]. The other fields
    /// are left false for the caller to fill in.
    pub fn switch_window(&self, wid: WindowId) -> SwitchWindow {
        SwitchWindow {
            wid,
            contexts: self.contexts_of(wid),
            pinned: self.is_pinned(wid),
            untracked: false,
            invisible: false,
            parked: false,
            own: false,
            last_focus: self.last_focus(wid),
        }
    }
}

/// The version of `contexts.json` that this code reads and writes.
pub const CONTEXTS_FILE_VERSION: u32 = 1;

/// The shape of `contexts.json`.
#[derive(Serialize, Deserialize)]
struct ContextsFile {
    version: u32,
    #[serde(default)]
    next_id: u32,
    #[serde(default)]
    use_seq: u64,
    #[serde(default)]
    contexts: Vec<Context>,
    #[serde(default)]
    pinned: Vec<MemberRecord>,
    #[serde(default)]
    active: Option<SavedActive>,
}

/// `{ "global": <key> }` or `{ "per_screen": { "<display id>": <key> } }`.
/// Anything else loads as Everything.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum SavedActive {
    Known(SavedActives),
    #[serde(skip_serializing)]
    Unknown(IgnoredAny),
}

#[derive(Serialize, Deserialize)]
struct SavedActives {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    global: Option<SavedKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    per_screen: Option<std::collections::BTreeMap<ScreenId, SavedKey>>,
}

/// A context id, or `"everything"` or `"unsorted"`.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum SavedKey {
    Named(ContextId),
    Builtin(BuiltinKey),
    #[serde(skip_serializing)]
    Unknown(IgnoredAny),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BuiltinKey {
    Everything,
    Unsorted,
}

/// A context key in the shape `contexts.json` stores it.
fn saved_key(key: ContextKey) -> SavedKey {
    match key {
        ContextKey::Everything => SavedKey::Builtin(BuiltinKey::Everything),
        ContextKey::Unsorted => SavedKey::Builtin(BuiltinKey::Unsorted),
        ContextKey::Named(id) => SavedKey::Named(id),
    }
}

/// Reads a stored key. A named context that doesn't exist, or a value of
/// another shape, loads as Everything.
fn read_saved_key(key: SavedKey, contexts: &Contexts) -> ContextKey {
    match key {
        SavedKey::Builtin(BuiltinKey::Unsorted) => ContextKey::Unsorted,
        SavedKey::Named(id) if contexts.get(id).is_some() => ContextKey::Named(id),
        _ => ContextKey::Everything,
    }
}

impl From<Contexts> for ContextsFile {
    fn from(contexts: Contexts) -> Self {
        let per_screen = (!contexts.screens.is_empty()).then(|| {
            contexts
                .screens
                .iter()
                .map(|(&screen, state)| (screen, saved_key(state.active)))
                .collect()
        });
        let active = SavedActives {
            global: per_screen.is_none().then(|| saved_key(contexts.active)),
            per_screen,
        };
        ContextsFile {
            version: CONTEXTS_FILE_VERSION,
            next_id: contexts.next_id,
            use_seq: contexts.use_seq,
            contexts: contexts.contexts,
            pinned: contexts.pinned,
            active: Some(SavedActive::Known(active)),
        }
    }
}

/// Reads a context number. A value that isn't an integer from 0 to 255
/// loads as no number.
fn saved_number<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SavedNumber {
        Integer(i64),
        Other(IgnoredAny),
    }
    Ok(match Option::<SavedNumber>::deserialize(deserializer)? {
        Some(SavedNumber::Integer(n)) => u8::try_from(n).ok(),
        Some(SavedNumber::Other(_)) | None => None,
    })
}

/// Loads `contexts.json`, repairing values that break the rules. A number
/// outside 1 to 9, or one that an earlier context has, is dropped. A
/// context whose id an earlier context has gets a fresh id and keeps its
/// members. An empty, reserved, or taken name gets a number (R4). Only
/// another version, or a file that isn't this shape, fails to load.
impl TryFrom<ContextsFile> for Contexts {
    type Error = String;

    fn try_from(file: ContextsFile) -> Result<Self, String> {
        if file.version != CONTEXTS_FILE_VERSION {
            return Err(format!("unsupported contexts.json version {}", file.version));
        }
        let max_id = file.contexts.iter().map(|c| c.id.0).max().unwrap_or(0);
        let max_used = file.contexts.iter().map(|c| c.last_used).max().unwrap_or(0);
        let mut contexts = Contexts {
            contexts: file.contexts,
            next_id: file.next_id.max(max_id.saturating_add(1)).max(1),
            use_seq: file.use_seq.max(max_used),
            pinned: file.pinned,
            ..Contexts::default()
        };
        for i in 0..contexts.contexts.len() {
            let id = contexts.contexts[i].id;
            if contexts.contexts[..i].iter().any(|c| c.id == id) {
                contexts.contexts[i].id = contexts.take_id();
            }
        }
        for mut context in std::mem::take(&mut contexts.contexts) {
            context.name = contexts.repaired_name(&context.name);
            if context
                .number
                .is_some_and(|n| !(1..=9).contains(&n) || contexts.by_number(n).is_some())
            {
                context.number = None;
            }
            contexts.contexts.push(context);
        }
        let (global, per_screen) = match file.active {
            Some(SavedActive::Known(active)) => (active.global, active.per_screen),
            Some(SavedActive::Unknown(_)) | None => (None, None),
        };
        contexts.active =
            global.map_or(ContextKey::Everything, |key| read_saved_key(key, &contexts));
        if let Some(per_screen) = per_screen {
            for (screen, key) in per_screen {
                let active = read_saved_key(key, &contexts);
                contexts.screens.insert(screen, ScreenState { active, previous: None });
            }
        }
        Ok(contexts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wid(pid: i32, idx: u32) -> WindowId {
        WindowId::new(pid, idx)
    }

    fn window(pid: i32, idx: u32, app: &str, title: &str) -> WindowDesc {
        WindowDesc {
            wid: wid(pid, idx),
            bundle_id: Some(format!("com.example.{app}")),
            app_name: Some(app.to_string()),
            title: title.to_string(),
            window_server_id: Some(WindowServerId(pid as u32 * 1000 + idx)),
        }
    }

    fn named(id: ContextId) -> ContextKey {
        ContextKey::Named(id)
    }

    #[test]
    fn r1_window_can_be_in_several_contexts() {
        let mut cx = Contexts::new();
        let comms = cx.create("Comms").unwrap();
        let relax = cx.create("Relax").unwrap();
        let whatsapp = window(1, 1, "WhatsApp", "WhatsApp");
        assert!(cx.add_window(comms, &whatsapp).unwrap());
        assert!(cx.add_window(relax, &whatsapp).unwrap());
        assert!(!cx.add_window(relax, &whatsapp).unwrap());
        assert_eq!(cx.contexts_of(whatsapp.wid), vec![comms, relax]);
        assert_eq!(cx.get(relax).unwrap().members.len(), 1);
        assert!(!cx.is_unsorted(whatsapp.wid));
        assert!(cx.is_unsorted(wid(2, 1)));
    }

    #[test]
    fn r3_pinned_window_is_member_of_every_context_including_later_ones() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let w = window(1, 1, "Music", "Music");
        assert!(cx.pin(&w));
        assert!(!cx.pin(&w));
        let b = cx.create("B").unwrap();
        assert!(cx.is_member(named(a), w.wid));
        assert!(cx.is_member(named(b), w.wid));
        assert!(cx.is_member(ContextKey::Everything, w.wid));
        // A pinned window shows under Unsorted too, but isn't counted as unsorted.
        assert!(cx.is_member(ContextKey::Unsorted, w.wid));
        assert!(!cx.is_unsorted(w.wid));
    }

    #[test]
    fn r3_unpinning_keeps_named_memberships() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let w = window(1, 1, "Music", "Music");
        cx.add_window(a, &w).unwrap();
        cx.pin(&w);
        assert!(cx.unpin(w.wid));
        assert!(!cx.unpin(w.wid));
        assert!(cx.is_member(named(a), w.wid));
        assert!(!cx.is_member(named(b), w.wid));
    }

    #[test]
    fn r4_names_are_unique_ignoring_case() {
        let mut cx = Contexts::new();
        let comms = cx.create("Comms").unwrap();
        assert_eq!(cx.create("comms"), Err(ContextError::NameTaken("Comms".into())));
        assert_eq!(cx.create("  "), Err(ContextError::EmptyName));
        let other = cx.create("Other").unwrap();
        assert_eq!(
            cx.rename(other, "COMMS"),
            Err(ContextError::NameTaken("Comms".into()))
        );
        cx.rename(comms, "COMMS").unwrap();
        assert_eq!(cx.get(comms).unwrap().name, "COMMS");
        assert_eq!(cx.by_name("comms").unwrap().id, comms);
    }

    #[test]
    fn r4_everything_and_unsorted_are_reserved() {
        let mut cx = Contexts::new();
        assert_eq!(
            cx.create("everything"),
            Err(ContextError::ReservedName("everything".into()))
        );
        assert_eq!(
            cx.create("Unsorted "),
            Err(ContextError::ReservedName("Unsorted".into()))
        );
        let a = cx.create("A").unwrap();
        assert_eq!(
            cx.rename(a, "UNSORTED"),
            Err(ContextError::ReservedName("UNSORTED".into()))
        );
    }

    #[test]
    fn r5_new_context_gets_lowest_free_number() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let c = cx.create("C").unwrap();
        assert_eq!(cx.get(b).unwrap().number, Some(2));
        cx.delete(b).unwrap();
        cx.set_number(a, None).unwrap();
        let d = cx.create("D").unwrap();
        let e = cx.create("E").unwrap();
        assert_eq!(cx.get(d).unwrap().number, Some(1));
        assert_eq!(cx.get(e).unwrap().number, Some(2));
        assert_eq!(cx.get(c).unwrap().number, Some(3));
    }

    #[test]
    fn r5_no_number_when_one_to_nine_are_taken() {
        let mut cx = Contexts::new();
        for n in 1..=9 {
            let id = cx.create(&format!("C{n}")).unwrap();
            assert_eq!(cx.get(id).unwrap().number, Some(n));
        }
        let tenth = cx.create("C10").unwrap();
        assert_eq!(cx.get(tenth).unwrap().number, None);
    }

    #[test]
    fn r5_numbers_stay_unique() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        cx.set_number(b, Some(1)).unwrap();
        assert_eq!(cx.get(a).unwrap().number, None);
        assert_eq!(cx.get(b).unwrap().number, Some(1));
        assert_eq!(cx.by_number(1).unwrap().id, b);
        assert_eq!(cx.set_number(a, Some(0)), Err(ContextError::NumberOutOfRange(0)));
        assert_eq!(
            cx.set_number(a, Some(10)),
            Err(ContextError::NumberOutOfRange(10))
        );
        assert_eq!(
            cx.set_number(ContextId(99), Some(3)),
            Err(ContextError::NoSuchContext)
        );
        assert_eq!(cx.get(b).unwrap().number, Some(1));
    }

    #[test]
    fn r6_deleting_context_leaves_its_only_members_unsorted() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let only_a = window(1, 1, "App", "One");
        let shared = window(1, 2, "App", "Two");
        cx.add_window(a, &only_a).unwrap();
        cx.add_window(a, &shared).unwrap();
        cx.add_window(b, &shared).unwrap();
        cx.switch_to(named(b)).unwrap();
        let deleted = cx.delete(a).unwrap();
        assert_eq!(deleted.members.len(), 2);
        assert!(cx.is_unsorted(only_a.wid));
        assert_eq!(cx.contexts_of(shared.wid), vec![b]);
        assert_eq!(cx.active(), named(b));
        assert_eq!(cx.delete(a), Err(ContextError::NoSuchContext));
    }

    #[test]
    fn r6_deleting_active_context_activates_unsorted() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        cx.switch_to(named(a)).unwrap();
        cx.delete(a).unwrap();
        assert_eq!(cx.active(), ContextKey::Unsorted);
    }

    #[test]
    fn r6_deleting_previous_context_clears_previous() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        cx.switch_to(named(a)).unwrap();
        cx.switch_to(named(b)).unwrap();
        assert_eq!(cx.previous(), Some(named(a)));
        cx.delete(a).unwrap();
        assert_eq!(cx.previous(), None);
    }

    #[test]
    fn r18_previous_context_is_the_one_used_before() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        assert_eq!(cx.previous(), None);
        cx.switch_to(named(a)).unwrap();
        assert_eq!(cx.previous(), Some(ContextKey::Everything));
        cx.switch_to(named(b)).unwrap();
        assert_eq!(cx.previous(), Some(named(a)));
        cx.switch_to(ContextKey::Unsorted).unwrap();
        assert_eq!(cx.previous(), Some(named(b)));
    }

    #[test]
    fn r16_switching_to_the_active_context_keeps_previous() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        cx.switch_to(named(a)).unwrap();
        cx.switch_to(named(b)).unwrap();
        cx.switch_to(named(b)).unwrap();
        assert_eq!(cx.active(), named(b));
        assert_eq!(cx.previous(), Some(named(a)));
    }

    #[test]
    fn r19_each_switch_updates_most_recently_used_order() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        assert_eq!(cx.last_used(named(a)), 0);
        cx.switch_to(named(a)).unwrap();
        cx.switch_to(named(b)).unwrap();
        assert!(cx.last_used(named(b)) > cx.last_used(named(a)));
        cx.switch_to(ContextKey::Unsorted).unwrap();
        cx.switch_to(named(a)).unwrap();
        assert!(cx.last_used(named(a)) > cx.last_used(ContextKey::Unsorted));
        assert!(cx.last_used(ContextKey::Unsorted) > cx.last_used(named(b)));
        assert_eq!(cx.last_used(ContextKey::Everything), 0);
        assert_eq!(
            cx.switch_to(named(ContextId(99))),
            Err(ContextError::NoSuchContext)
        );
    }

    #[test]
    fn r24_focus_target_is_most_recently_used_context_holding_the_window() {
        let mut cx = Contexts::new();
        let comms = cx.create("Comms").unwrap();
        let relax = cx.create("Relax").unwrap();
        let work = cx.create("Work").unwrap();
        let whatsapp = window(1, 1, "WhatsApp", "WhatsApp");
        cx.add_window(comms, &whatsapp).unwrap();
        cx.add_window(relax, &whatsapp).unwrap();
        // Under Everything, focus never switches.
        assert_eq!(cx.focus_target(whatsapp.wid), ContextKey::Everything);
        cx.switch_to(named(relax)).unwrap();
        cx.switch_to(named(comms)).unwrap();
        cx.switch_to(named(work)).unwrap();
        assert_eq!(cx.focus_target(whatsapp.wid), named(comms));
        assert_eq!(cx.focus_target(wid(2, 1)), ContextKey::Unsorted);
        cx.switch_to(named(relax)).unwrap();
        assert_eq!(cx.focus_target(whatsapp.wid), named(relax));
    }

    #[test]
    fn focus_order_is_a_sequence() {
        let mut cx = Contexts::new();
        assert_eq!(cx.last_focus(wid(1, 1)), None);
        cx.window_focused(wid(1, 1));
        cx.window_focused(wid(1, 2));
        assert!(cx.last_focus(wid(1, 2)) > cx.last_focus(wid(1, 1)));
    }

    #[test]
    fn r28_everything_is_active_until_a_switch() {
        let mut cx = Contexts::new();
        assert_eq!(cx.active(), ContextKey::Everything);
        cx.create("A").unwrap();
        assert_eq!(cx.active(), ContextKey::Everything);
    }

    #[test]
    fn move_window_leaves_the_active_context() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let c = cx.create("C").unwrap();
        let w = window(1, 1, "App", "W");
        cx.add_window(a, &w).unwrap();
        cx.add_window(c, &w).unwrap();
        cx.switch_to(named(a)).unwrap();
        cx.move_window(named(a), b, &w).unwrap();
        assert_eq!(cx.contexts_of(w.wid), vec![b, c]);
        assert_eq!(
            cx.move_window(named(a), ContextId(99), &w),
            Err(ContextError::NoSuchContext)
        );
    }

    #[test]
    fn move_window_under_everything_only_adds() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let w = window(1, 1, "App", "W");
        cx.add_window(a, &w).unwrap();
        cx.move_window(ContextKey::Everything, b, &w).unwrap();
        assert_eq!(cx.contexts_of(w.wid), vec![a, b]);
    }

    #[test]
    fn remove_window_removes_one_membership() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let w = window(1, 1, "App", "W");
        cx.add_window(a, &w).unwrap();
        cx.add_window(b, &w).unwrap();
        assert!(cx.remove_window(a, w.wid).unwrap());
        assert!(!cx.remove_window(a, w.wid).unwrap());
        assert_eq!(cx.contexts_of(w.wid), vec![b]);
    }

    fn ranked_names(query: &str, cx: &Contexts, unsorted: bool) -> Vec<String> {
        rank(query, cx, unsorted)
            .into_iter()
            .map(|(key, _)| match key {
                ContextKey::Everything => EVERYTHING_NAME.to_string(),
                ContextKey::Unsorted => UNSORTED_NAME.to_string(),
                ContextKey::Named(id) => cx.get(id).unwrap().name.clone(),
            })
            .collect()
    }

    fn match_of(query: &str, name: &str) -> Option<NameMatch> {
        match_name(&fold(query), &fold(name))
    }

    #[test]
    fn rank_prefix_finds_client_work() {
        let mut cx = Contexts::new();
        cx.create("Comms").unwrap();
        cx.create("Client work").unwrap();
        assert_eq!(ranked_names("cli", &cx, false), vec!["Client work"]);
        assert_eq!(match_of("cli", "Client work"), Some(NameMatch::NamePrefix));
    }

    #[test]
    fn rank_initials_find_client_work() {
        let mut cx = Contexts::new();
        cx.create("Comms").unwrap();
        cx.create("Client work").unwrap();
        assert_eq!(ranked_names("cw", &cx, false), vec!["Client work"]);
        assert_eq!(match_of("cw", "Client work"), Some(NameMatch::Initials));
    }

    #[test]
    fn rank_exact_name_beats_prefix() {
        let mut cx = Contexts::new();
        let longer = cx.create("Client work 2").unwrap();
        cx.create("Client work").unwrap();
        cx.switch_to(ContextKey::Named(longer)).unwrap();
        assert_eq!(
            ranked_names("client work", &cx, false),
            vec!["Client work", "Client work 2"]
        );
    }

    #[test]
    fn rank_ignores_accents_and_case() {
        let mut cx = Contexts::new();
        cx.create("Café").unwrap();
        cx.create("Straße").unwrap();
        assert_eq!(ranked_names("cafe", &cx, false), vec!["Café"]);
        assert_eq!(ranked_names("CAFÉ", &cx, false), vec!["Café"]);
        assert_eq!(ranked_names("strasse", &cx, false), vec!["Straße"]);
        // Decomposed accents, as in some file names.
        assert_eq!(ranked_names("Cafe\u{301}", &cx, false), vec!["Café"]);
    }

    #[test]
    fn rank_ties_go_to_most_recently_used() {
        let mut cx = Contexts::new();
        let client = cx.create("Client").unwrap();
        let comms = cx.create("Comms").unwrap();
        cx.switch_to(ContextKey::Named(client)).unwrap();
        cx.switch_to(ContextKey::Named(comms)).unwrap();
        assert_eq!(ranked_names("c", &cx, false), vec!["Comms", "Client"]);
        cx.switch_to(ContextKey::Named(client)).unwrap();
        assert_eq!(ranked_names("c", &cx, false), vec!["Client", "Comms"]);
    }

    #[test]
    fn rank_orders_match_kinds() {
        assert_eq!(match_of("client work", "Client work"), Some(NameMatch::Exact));
        assert_eq!(match_of("client", "Client work"), Some(NameMatch::NamePrefix));
        assert_eq!(match_of("wor", "Client work"), Some(NameMatch::WordPrefix));
        assert_eq!(match_of("cwp", "Client work party"), Some(NameMatch::Initials));
        assert_eq!(
            match_of("cl wo", "Client work"),
            Some(NameMatch::AllWordPrefixes)
        );
        assert_eq!(match_of("clwk", "Client work"), Some(NameMatch::LettersInOrder));
        assert_eq!(match_of("wc", "Client work"), None);
        assert_eq!(
            match_of("wo cl", "Client work"),
            Some(NameMatch::AllWordPrefixes)
        );
        let mut kinds = vec![
            NameMatch::LettersInOrder,
            NameMatch::Exact,
            NameMatch::Initials,
            NameMatch::NamePrefix,
            NameMatch::AllWordPrefixes,
            NameMatch::WordPrefix,
        ];
        kinds.sort();
        kinds.reverse();
        assert_eq!(
            kinds,
            vec![
                NameMatch::Exact,
                NameMatch::NamePrefix,
                NameMatch::WordPrefix,
                NameMatch::Initials,
                NameMatch::AllWordPrefixes,
                NameMatch::LettersInOrder,
            ]
        );
    }

    #[test]
    fn r29_rank_lists_unsorted_only_when_it_has_windows() {
        let mut cx = Contexts::new();
        cx.create("Comms").unwrap();
        assert_eq!(ranked_names("", &cx, false), vec!["Comms", "Everything"]);
        assert_eq!(
            ranked_names("", &cx, true),
            vec!["Comms", "Unsorted", "Everything"]
        );
        assert_eq!(ranked_names("uns", &cx, true), vec!["Unsorted"]);
        assert_eq!(ranked_names("uns", &cx, false), Vec::<String>::new());
        assert_eq!(ranked_names("ev", &cx, true), vec!["Everything"]);
    }

    #[test]
    fn fold_covers_latin_1_and_extended_a() {
        assert_eq!(fold("ÀÉÎÕÜ àéîõü Çç Ññ Ýÿ"), "aeiou aeiou cc nn yy");
        assert_eq!(fold("Æ Œ ß Þ Ð Ø Ĳ"), "ae oe ss th d o ij");
        assert_eq!(fold("Łódź Škoda İstanbul ıi Ħ ſ"), "lodz skoda istanbul ii h s");
        assert_eq!(fold("×÷ 日本"), "×÷ 日本");
    }

    fn empty_record(app: &str, title: &str, wsid: Option<u32>) -> MemberRecord {
        MemberRecord {
            bundle_id: Some(format!("com.example.{app}")),
            app_name: Some(app.to_string()),
            title: title.to_string(),
            window_server_id: wsid.map(WindowServerId),
            link: RecordLink::Empty,
        }
    }

    fn with_records(names_and_records: &[(&str, Vec<MemberRecord>)]) -> (Contexts, Vec<ContextId>) {
        let mut cx = Contexts::new();
        let ids = names_and_records
            .iter()
            .map(|(name, records)| {
                let id = cx.create(name).unwrap();
                cx.get_mut(id).unwrap().members = records.clone();
                id
            })
            .collect();
        (cx, ids)
    }

    fn steps(matches: &[RecordMatch]) -> Vec<(Slot, MatchStep)> {
        matches.iter().map(|m| (m.slot, m.step)).collect()
    }

    #[test]
    fn r22_similar_titles_follow_the_rooms_definition() {
        assert!(similar_titles("Inbox – Mail", "inbox"));
        assert!(similar_titles("Café Notes", "cafe notes (edited)"));
        assert!(!similar_titles("abc", "abc"), "shorter than 4 characters");
        assert!(!similar_titles("Mail", "abc"));
        // Shorter title has 9 characters; two-thirds is 6.
        assert!(similar_titles("Project A", "Projec-99999"));
        assert!(!similar_titles("Project A", "Proje-999999"));
        // A shared prefix of 12 is always enough.
        assert!(similar_titles(
            "Quarterly report draft 1",
            "Quarterly re-something else entirely"
        ));
        assert!(!similar_titles(
            "Quarterly report draft 1",
            "Quarterly r-something else entirely"
        ));
    }

    #[test]
    fn r22_step_1_matches_window_server_id() {
        let (cx, ids) = with_records(&[
            ("A", vec![empty_record("App", "Unrelated", Some(1001))]),
            ("B", vec![empty_record("Other", "Unrelated", Some(1001))]),
        ]);
        let w = window(1, 1, "App", "Title");
        // B's record has the same window server id but another app.
        assert_eq!(
            steps(&match_window(&w, &cx, MatchPass::Arrival)),
            vec![(Slot::Context(ids[0]), MatchStep::WindowServerId)]
        );
    }

    #[test]
    fn r22_step_2_matches_same_app_and_exact_title() {
        let (cx, ids) = with_records(&[
            ("A", vec![empty_record("App", "Title", None)]),
            ("B", vec![empty_record("Other", "Title", None)]),
        ]);
        let w = window(1, 1, "App", "Title");
        assert_eq!(
            steps(&match_window(&w, &cx, MatchPass::Arrival)),
            vec![(Slot::Context(ids[0]), MatchStep::ExactTitle)]
        );
    }

    #[test]
    fn r22_step_3_matches_same_app_and_similar_title() {
        let (cx, ids) = with_records(&[("A", vec![empty_record("App", "Inbox – Mail", None)])]);
        let w = window(1, 1, "App", "Inbox");
        assert_eq!(
            steps(&match_window(&w, &cx, MatchPass::Arrival)),
            vec![(Slot::Context(ids[0]), MatchStep::SimilarTitle)]
        );
        let unrelated = window(1, 2, "App", "Drafts");
        assert!(match_window(&unrelated, &cx, MatchPass::Arrival).is_empty());
    }

    #[test]
    fn r22_step_4_only_runs_during_a_switch() {
        let (cx, ids) = with_records(&[("A", vec![empty_record("App", "Something", None)])]);
        let w = window(1, 1, "App", "Different");
        let to_a = MatchPass::Switch { target: named(ids[0]) };
        assert!(match_window(&w, &cx, MatchPass::Arrival).is_empty());
        assert_eq!(
            steps(&match_window(&w, &cx, to_a)),
            vec![(Slot::Context(ids[0]), MatchStep::SameApp)]
        );
    }

    #[test]
    fn r22_earlier_steps_win() {
        let (cx, ids) = with_records(&[
            (
                "A",
                vec![
                    empty_record("App", "Inbox – Mail", None),
                    empty_record("App", "Inbox", None),
                ],
            ),
            ("B", vec![empty_record("App", "Inbox – Mail", None)]),
        ]);
        let w = window(1, 1, "App", "Inbox");
        let matches = match_window(&w, &cx, MatchPass::Switch { target: named(ids[1]) });
        // A's exact record wins; B's similar record isn't taken because the
        // window now belongs to A.
        assert_eq!(
            steps(&matches),
            vec![(Slot::Context(ids[0]), MatchStep::ExactTitle)]
        );
        assert_eq!(matches[0].index, 1);
    }

    #[test]
    fn r22_rejoins_every_context_that_holds_a_matching_record() {
        let (mut cx, ids) = with_records(&[
            ("Comms", vec![empty_record("WhatsApp", "WhatsApp", None)]),
            ("Relax", vec![empty_record("WhatsApp", "WhatsApp", None)]),
            ("Work", vec![]),
        ]);
        let w = window(1, 1, "WhatsApp", "WhatsApp");
        assert_eq!(
            cx.window_appeared(&w, named(ids[2])),
            Arrival::Rejoined(vec![
                RecordMatch {
                    slot: Slot::Context(ids[0]),
                    index: 0,
                    step: MatchStep::ExactTitle
                },
                RecordMatch {
                    slot: Slot::Context(ids[1]),
                    index: 0,
                    step: MatchStep::ExactTitle
                },
            ])
        );
        assert_eq!(cx.contexts_of(w.wid), vec![ids[0], ids[1]]);
        assert_eq!(
            cx.get(ids[0]).unwrap().members[0].window_server_id,
            w.window_server_id
        );
    }

    #[test]
    fn r22_steps_3_and_4_never_take_a_window_of_another_context() {
        let (mut cx, ids) = with_records(&[
            ("A", vec![]),
            (
                "B",
                vec![
                    empty_record("App", "Inbox – Mail", None),
                    empty_record("App", "Other", None),
                ],
            ),
        ]);
        let w = window(1, 1, "App", "Inbox");
        let to_b = MatchPass::Switch { target: named(ids[1]) };
        cx.add_window(ids[0], &w).unwrap();
        assert!(match_window(&w, &cx, MatchPass::Arrival).is_empty());
        assert!(match_window(&w, &cx, to_b).is_empty());
        // A pinned window belongs to every context.
        cx.remove_window(ids[0], w.wid).unwrap();
        cx.pin(&w);
        assert!(match_window(&w, &cx, to_b).is_empty());
        cx.unpin(w.wid);
        assert_eq!(match_window(&w, &cx, to_b).len(), 1);
    }

    #[test]
    fn r22_a_record_binds_one_window() {
        let (mut cx, ids) = with_records(&[("A", vec![empty_record("App", "Title", None)])]);
        let first = window(1, 1, "App", "Title");
        let second = window(1, 2, "App", "Title");
        let to_a = MatchPass::Switch { target: named(ids[0]) };
        assert_eq!(cx.rejoin(&first, to_a).len(), 1);
        assert!(cx.rejoin(&second, to_a).is_empty());
        assert!(cx.rejoin(&first, to_a).is_empty());
        assert_eq!(cx.contexts_of(first.wid), vec![ids[0]]);
        assert_eq!(cx.get(ids[0]).unwrap().members.len(), 1);
    }

    #[test]
    fn r22_pinned_record_rejoins_as_pinned() {
        let mut cx = Contexts::new();
        cx.pinned.push(empty_record("Music", "Music", None));
        let w = window(1, 1, "Music", "Music");
        let matches = cx.rejoin(&w, MatchPass::Arrival);
        assert_eq!(steps(&matches), vec![(Slot::Pinned, MatchStep::ExactTitle)]);
        assert!(cx.is_pinned(w.wid));
    }

    #[test]
    fn r22_record_title_follows_its_window() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let w = window(1, 1, "Chrome", "New Tab");
        cx.add_window(a, &w).unwrap();
        cx.pin(&w);
        cx.title_changed(w.wid, "Docs – Q3 plan");
        assert_eq!(cx.get(a).unwrap().members[0].title, "Docs – Q3 plan");
        assert_eq!(cx.pinned()[0].title, "Docs – Q3 plan");
    }

    #[test]
    fn r22_relaunched_window_with_the_last_title_rejoins_at_step_2() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let w = window(1, 1, "Chrome", "New Tab");
        cx.add_window(a, &w).unwrap();
        cx.title_changed(w.wid, "Docs – Q3 plan");
        cx.app_terminated(1);
        // After a relaunch the window has a new pid and window server id.
        let relaunched = WindowDesc {
            window_server_id: Some(WindowServerId(5555)),
            ..window(2, 7, "Chrome", "Docs – Q3 plan")
        };
        assert_eq!(
            steps(&cx.rejoin(&relaunched, MatchPass::Arrival)),
            vec![(Slot::Context(a), MatchStep::ExactTitle)]
        );
        assert_eq!(cx.contexts_of(relaunched.wid), vec![a]);
    }

    #[test]
    fn r20_unmatched_new_window_joins_the_active_context() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let w = window(1, 1, "App", "New");
        assert_eq!(cx.window_appeared(&w, named(a)), Arrival::Joined(a));
        assert_eq!(cx.contexts_of(w.wid), vec![a]);
    }

    #[test]
    fn r20_unmatched_new_window_under_everything_or_unsorted_is_unsorted() {
        let mut cx = Contexts::new();
        cx.create("A").unwrap();
        let w1 = window(1, 1, "App", "One");
        let w2 = window(1, 2, "App", "Two");
        assert_eq!(
            cx.window_appeared(&w1, ContextKey::Everything),
            Arrival::Unsorted
        );
        assert_eq!(cx.window_appeared(&w2, ContextKey::Unsorted), Arrival::Unsorted);
        assert!(cx.is_unsorted(w1.wid));
        assert!(cx.is_unsorted(w2.wid));
    }

    #[test]
    fn r21_matched_window_does_not_join_the_active_context() {
        let (mut cx, ids) = with_records(&[
            ("A", vec![empty_record("App", "Title", None)]),
            ("B", vec![]),
        ]);
        let w = window(1, 1, "App", "Title");
        assert!(matches!(
            cx.window_appeared(&w, named(ids[1])),
            Arrival::Rejoined(_)
        ));
        assert_eq!(cx.contexts_of(w.wid), vec![ids[0]]);
    }

    #[test]
    fn r20_a_repeated_arrival_changes_nothing() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let w = window(1, 1, "App", "Title");
        assert_eq!(cx.window_appeared(&w, named(a)), Arrival::Joined(a));
        assert_eq!(cx.window_appeared(&w, named(a)), Arrival::AlreadyMember);
        assert_eq!(cx.window_appeared(&w, named(b)), Arrival::AlreadyMember);
        assert_eq!(cx.contexts_of(w.wid), vec![a]);
        assert_eq!(cx.get(a).unwrap().members.len(), 1);
    }

    #[test]
    fn r23_pending_records_match_nothing() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let w = window(1, 1, "App", "Title");
        cx.add_window(a, &w).unwrap();
        cx.window_closed(w.wid);
        assert_eq!(cx.get(a).unwrap().members[0].link, RecordLink::Pending(w.wid));
        assert!(cx.is_unsorted(w.wid));
        let to_a = MatchPass::Switch { target: named(a) };
        let same = window(1, 1, "App", "Title");
        assert!(match_window(&same, &cx, to_a).is_empty());
        let other = window(1, 2, "App", "Title");
        assert!(match_window(&other, &cx, to_a).is_empty());
    }

    #[test]
    fn r23_pending_records_stay_when_the_app_terminates() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let w = window(1, 1, "App", "Title");
        cx.add_window(a, &w).unwrap();
        cx.pin(&w);
        cx.window_closed(w.wid);
        cx.app_terminated(1);
        assert_eq!(cx.get(a).unwrap().members[0].link, RecordLink::Empty);
        assert_eq!(cx.pinned()[0].link, RecordLink::Empty);
        let relaunched = window(2, 1, "App", "Title");
        assert_eq!(cx.rejoin(&relaunched, MatchPass::Arrival).len(), 2);
    }

    #[test]
    fn r23_open_windows_records_stay_when_the_app_terminates() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let w = window(1, 1, "App", "Title");
        let other_app = window(2, 1, "Other", "Title");
        cx.add_window(a, &w).unwrap();
        cx.add_window(a, &other_app).unwrap();
        cx.app_terminated(1);
        let members = &cx.get(a).unwrap().members;
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].link, RecordLink::Empty);
        assert_eq!(members[1].link, RecordLink::Live(other_app.wid));
    }

    #[test]
    fn r23_pending_records_go_when_the_app_is_still_running() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let closed = window(1, 1, "App", "Closed");
        let open = window(1, 2, "App", "Open");
        let other_app = window(2, 1, "Other", "Closed");
        for w in [&closed, &open, &other_app] {
            cx.add_window(a, w).unwrap();
        }
        cx.pin(&closed);
        cx.window_closed(closed.wid);
        cx.window_closed(other_app.wid);
        cx.app_still_running(1);
        let members = &cx.get(a).unwrap().members;
        assert_eq!(
            members.iter().map(|m| m.title.as_str()).collect::<Vec<_>>(),
            vec!["Open", "Closed"]
        );
        assert_eq!(members[1].link, RecordLink::Pending(other_app.wid));
        assert!(cx.pinned().is_empty());
    }

    fn member_of(wid: WindowId, contexts: &[ContextId]) -> SwitchWindow {
        SwitchWindow {
            wid,
            contexts: contexts.to_vec(),
            pinned: false,
            untracked: false,
            invisible: false,
            parked: false,
            own: false,
            last_focus: None,
        }
    }

    fn one_screen(active: ContextKey, windows: Vec<SwitchWindow>) -> SwitchInput {
        SwitchInput {
            screens: vec![SwitchScreen { active, windows }],
            only: None,
        }
    }

    const A: ContextId = ContextId(1);
    const B: ContextId = ContextId(2);

    #[test]
    fn r12_r15_switch_parks_exactly_the_windows_that_must_not_show() {
        // One app has a window inside and a window outside the target.
        let plan = plan_switch(&one_screen(
            ContextKey::Named(A),
            vec![
                member_of(wid(1, 1), &[A]),
                member_of(wid(1, 2), &[B]),
                member_of(wid(2, 1), &[A, B]),
                member_of(wid(3, 1), &[]),
            ],
        ));
        assert_eq!(plan.park, vec![wid(1, 2), wid(3, 1)]);
        assert_eq!(plan.unpark, vec![]);
    }

    #[test]
    fn r12_parked_members_are_put_back() {
        let plan = plan_switch(&one_screen(
            ContextKey::Named(B),
            vec![
                SwitchWindow {
                    parked: true,
                    ..member_of(wid(1, 1), &[B])
                },
                SwitchWindow {
                    parked: true,
                    ..member_of(wid(1, 2), &[A])
                },
                member_of(wid(1, 3), &[A]),
            ],
        ));
        assert_eq!(plan.unpark, vec![wid(1, 1)]);
        assert_eq!(plan.park, vec![wid(1, 3)]);
    }

    #[test]
    fn r12_focuses_the_most_recently_focused_member() {
        let plan = plan_switch(&one_screen(
            ContextKey::Named(A),
            vec![
                SwitchWindow {
                    last_focus: Some(3),
                    ..member_of(wid(1, 1), &[A])
                },
                SwitchWindow {
                    last_focus: Some(9),
                    ..member_of(wid(1, 2), &[B])
                },
                SwitchWindow {
                    last_focus: Some(5),
                    parked: true,
                    ..member_of(wid(1, 3), &[A])
                },
                member_of(wid(1, 4), &[A]),
            ],
        ));
        assert_eq!(plan.focus, Some(wid(1, 3)));
    }

    #[test]
    fn r12_focus_never_lands_on_windows_the_user_cant_use() {
        let windows = vec![
            SwitchWindow {
                last_focus: Some(1),
                ..member_of(wid(1, 1), &[A])
            },
            SwitchWindow {
                last_focus: Some(2),
                own: true,
                ..member_of(wid(1, 2), &[A])
            },
            SwitchWindow {
                last_focus: Some(3),
                untracked: true,
                ..member_of(wid(1, 3), &[A])
            },
            SwitchWindow {
                last_focus: Some(4),
                invisible: true,
                ..member_of(wid(1, 4), &[A])
            },
        ];
        let plan = plan_switch(&one_screen(ContextKey::Named(A), windows));
        assert_eq!(plan.focus, Some(wid(1, 1)));
        let plan = plan_switch(&one_screen(
            ContextKey::Named(A),
            vec![member_of(wid(1, 2), &[B])],
        ));
        assert_eq!(plan.focus, None);
    }

    #[test]
    fn r13_pinned_windows_show_under_every_context() {
        let pinned = SwitchWindow {
            pinned: true,
            parked: true,
            ..member_of(wid(1, 1), &[])
        };
        for active in [
            ContextKey::Named(A),
            ContextKey::Unsorted,
            ContextKey::Everything,
        ] {
            let plan = plan_switch(&one_screen(active, vec![pinned.clone()]));
            assert_eq!(plan.unpark, vec![wid(1, 1)]);
            assert_eq!(plan.park, vec![]);
        }
    }

    #[test]
    fn r13_unsorted_shows_windows_in_no_context() {
        let plan = plan_switch(&one_screen(
            ContextKey::Unsorted,
            vec![member_of(wid(1, 1), &[]), member_of(wid(1, 2), &[A])],
        ));
        assert_eq!(plan.park, vec![wid(1, 2)]);
    }

    #[test]
    fn r14_never_parks_own_untracked_or_invisible_windows() {
        let windows = vec![
            SwitchWindow {
                own: true,
                ..member_of(wid(1, 1), &[])
            },
            SwitchWindow {
                untracked: true,
                ..member_of(wid(1, 2), &[])
            },
            SwitchWindow {
                invisible: true,
                ..member_of(wid(1, 3), &[])
            },
            member_of(wid(1, 4), &[]),
        ];
        let plan = plan_switch(&one_screen(ContextKey::Named(A), windows));
        assert_eq!(plan.park, vec![wid(1, 4)]);
    }

    #[test]
    fn r16_reapplying_the_active_context_parks_windows_that_drifted_in() {
        let plan = plan_switch(&one_screen(
            ContextKey::Named(A),
            vec![
                member_of(wid(1, 1), &[A]),
                SwitchWindow {
                    parked: true,
                    ..member_of(wid(1, 2), &[B])
                },
                member_of(wid(1, 3), &[B]),
            ],
        ));
        assert_eq!(plan.park, vec![wid(1, 3)]);
        assert_eq!(plan.unpark, vec![]);
    }

    #[test]
    fn r7_a_global_switch_changes_every_screen() {
        let input = SwitchInput {
            screens: vec![
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![member_of(wid(1, 1), &[A]), member_of(wid(1, 2), &[B])],
                },
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![
                        member_of(wid(2, 1), &[B]),
                        SwitchWindow {
                            parked: true,
                            ..member_of(wid(2, 2), &[A])
                        },
                    ],
                },
            ],
            only: None,
        };
        let plan = plan_switch(&input);
        assert_eq!(plan.park, vec![wid(1, 2), wid(2, 1)]);
        assert_eq!(plan.unpark, vec![wid(2, 2)]);
    }

    /// R8. In `per_screen` scope only the screen that switched changes: its
    /// own non-members are parked, and the members of its context come to it
    /// from the other screens.
    #[test]
    fn r8_a_per_screen_switch_takes_the_targets_members_to_that_screen() {
        let input = SwitchInput {
            screens: vec![
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![member_of(wid(1, 1), &[A]), member_of(wid(1, 2), &[B])],
                },
                SwitchScreen {
                    active: ContextKey::Named(B),
                    windows: vec![
                        member_of(wid(2, 1), &[A]),
                        member_of(wid(2, 2), &[B]),
                        SwitchWindow {
                            pinned: true,
                            ..member_of(wid(2, 3), &[])
                        },
                    ],
                },
            ],
            only: Some(0),
        };

        let plan = plan_switch(&input);

        assert_eq!(plan.park, vec![wid(1, 2)]);
        assert_eq!(plan.unpark, vec![]);
        // The other screen's member of A comes along. Its own member of B
        // stays, and the pinned window stays on both screens.
        assert_eq!(plan.moves, vec![SwitchMove { wid: wid(2, 1), screen: 0 }]);
    }

    /// R8. A parked member of the target on another screen moves and shows
    /// again, so it is put back as well as moved.
    #[test]
    fn r8_a_parked_member_of_another_screen_is_moved_and_put_back() {
        let input = SwitchInput {
            screens: vec![
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![],
                },
                SwitchScreen {
                    active: ContextKey::Named(B),
                    windows: vec![SwitchWindow {
                        parked: true,
                        ..member_of(wid(2, 1), &[A])
                    }],
                },
            ],
            only: Some(0),
        };

        let plan = plan_switch(&input);

        assert_eq!(plan.moves, vec![SwitchMove { wid: wid(2, 1), screen: 0 }]);
        assert_eq!(plan.unpark, vec![wid(2, 1)]);
        assert_eq!(plan.park, vec![]);
    }

    #[test]
    fn r8_a_switch_does_not_reconcile_windows_that_stay_on_another_screen() {
        let input = SwitchInput {
            screens: vec![
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![],
                },
                SwitchScreen {
                    active: ContextKey::Named(B),
                    windows: vec![
                        member_of(wid(2, 1), &[]),
                        SwitchWindow {
                            parked: true,
                            ..member_of(wid(2, 2), &[B])
                        },
                    ],
                },
            ],
            only: Some(0),
        };

        let plan = plan_switch(&input);
        assert!(plan.park.is_empty());
        assert!(plan.unpark.is_empty());
        assert!(plan.moves.is_empty());
    }

    /// R14. A window the user can't use stays where it is, whatever context
    /// it is a member of.
    #[test]
    fn r8_r14_a_window_that_cant_be_used_is_not_moved() {
        let input = SwitchInput {
            screens: vec![
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![],
                },
                SwitchScreen {
                    active: ContextKey::Named(B),
                    windows: vec![
                        SwitchWindow {
                            own: true,
                            ..member_of(wid(2, 1), &[A])
                        },
                        SwitchWindow {
                            untracked: true,
                            ..member_of(wid(2, 2), &[A])
                        },
                        SwitchWindow {
                            invisible: true,
                            ..member_of(wid(2, 3), &[A])
                        },
                    ],
                },
            ],
            only: Some(0),
        };

        let plan = plan_switch(&input);

        assert_eq!(plan.moves, vec![]);
        assert_eq!(plan.park, vec![]);
    }

    /// R12. A per-screen switch focuses a window that shows on the screen it
    /// changed, or one that moves there, and never one that stays elsewhere.
    #[test]
    fn r12_a_per_screen_switch_focuses_on_the_screen_that_switched() {
        let input = SwitchInput {
            screens: vec![
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![SwitchWindow {
                        last_focus: Some(5),
                        ..member_of(wid(1, 1), &[A])
                    }],
                },
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![
                        SwitchWindow {
                            last_focus: Some(9),
                            ..member_of(wid(2, 1), &[A])
                        },
                        SwitchWindow {
                            last_focus: Some(20),
                            ..member_of(wid(2, 2), &[B])
                        },
                    ],
                },
            ],
            only: Some(0),
        };

        let plan = plan_switch(&input);

        assert_eq!(plan.focus, Some(wid(2, 1)));
        assert_eq!(plan.moves, vec![SwitchMove { wid: wid(2, 1), screen: 0 }]);
    }

    /// R9. A window that two screens' contexts share goes to whichever
    /// screen switched last.
    #[test]
    fn r9_a_shared_window_goes_to_the_screen_that_switched_last() {
        let shared = member_of(wid(1, 1), &[A, B]);
        let right_switches = SwitchInput {
            screens: vec![
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![shared.clone()],
                },
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![],
                },
            ],
            only: Some(1),
        };
        let plan = plan_switch(&right_switches);
        assert_eq!(plan.moves, vec![SwitchMove { wid: wid(1, 1), screen: 1 }]);

        // The window is on the right screen now. The left screen switches,
        // and takes it back.
        let left_switches = SwitchInput {
            screens: vec![
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![],
                },
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![shared],
                },
            ],
            only: Some(0),
        };
        let plan = plan_switch(&left_switches);
        assert_eq!(plan.moves, vec![SwitchMove { wid: wid(1, 1), screen: 0 }]);
        assert_eq!(plan.park, vec![]);
    }

    /// R27. Showing Everything on one screen moves nothing: everything is a
    /// member of nothing and shows everywhere it is active.
    #[test]
    fn r27_showing_everything_on_one_screen_moves_nothing() {
        let input = SwitchInput {
            screens: vec![
                SwitchScreen {
                    active: ContextKey::Everything,
                    windows: vec![member_of(wid(1, 1), &[A])],
                },
                SwitchScreen {
                    active: ContextKey::Named(B),
                    windows: vec![member_of(wid(2, 1), &[A, B])],
                },
            ],
            only: Some(0),
        };

        let plan = plan_switch(&input);

        assert_eq!(plan.moves, vec![]);
        assert_eq!(plan.park, vec![]);
    }

    #[test]
    fn r13_windows_in_no_context_are_parked_under_a_named_context() {
        let plan = plan_switch(&one_screen(
            ContextKey::Named(A),
            vec![member_of(wid(4, 1), &[A]), member_of(wid(4, 2), &[])],
        ));
        assert_eq!(plan.park, vec![wid(4, 2)]);
    }

    #[test]
    fn r27_everything_parks_nothing_and_puts_back_every_parked_window() {
        let windows = vec![
            member_of(wid(1, 1), &[A]),
            SwitchWindow {
                parked: true,
                ..member_of(wid(1, 2), &[B])
            },
            SwitchWindow {
                parked: true,
                ..member_of(wid(1, 3), &[])
            },
            SwitchWindow {
                parked: true,
                invisible: true,
                ..member_of(wid(1, 4), &[])
            },
            SwitchWindow {
                parked: true,
                untracked: true,
                ..member_of(wid(1, 5), &[])
            },
        ];
        let plan = plan_switch(&one_screen(ContextKey::Everything, windows));
        assert_eq!(plan.park, vec![]);
        assert_eq!(plan.unpark, vec![wid(1, 2), wid(1, 3), wid(1, 4), wid(1, 5)]);
    }

    #[test]
    fn r20_an_unmatched_new_window_is_never_parked() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        cx.switch_to(named(a)).unwrap();
        let w = window(1, 1, "App", "New");
        cx.window_appeared(&w, cx.active());
        let plan = plan_switch(&one_screen(cx.active(), vec![cx.switch_window(w.wid)]));
        assert_eq!(plan.park, vec![]);
    }

    #[test]
    fn r21_a_rejoined_window_outside_the_active_context_is_parked() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let w = window(1, 1, "App", "Title");
        cx.add_window(a, &w).unwrap();
        cx.app_terminated(1);
        cx.switch_to(named(b)).unwrap();
        let relaunched = window(2, 1, "App", "Title");
        assert!(matches!(
            cx.window_appeared(&relaunched, cx.active()),
            Arrival::Rejoined(_)
        ));
        let plan = plan_switch(&one_screen(cx.active(), vec![cx.switch_window(relaunched.wid)]));
        assert_eq!(plan.park, vec![relaunched.wid]);
    }

    #[test]
    fn switch_window_describes_membership_and_focus() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let w = window(1, 1, "App", "Title");
        cx.add_window(a, &w).unwrap();
        cx.pin(&w);
        cx.window_focused(w.wid);
        let described = cx.switch_window(w.wid);
        assert_eq!(described.contexts, vec![a]);
        assert!(described.pinned);
        assert_eq!(described.last_focus, Some(1));
    }

    const SPEC_EXAMPLE: &str = r#"{
  "version": 1,
  "next_id": 3,
  "use_seq": 42,
  "contexts": [
    { "id": 1, "name": "Comms", "number": 1, "last_used": 42,
      "members": [
        { "bundle_id": "net.whatsapp.WhatsApp", "app_name": "WhatsApp", "title": "WhatsApp", "window_server_id": 81234 }
      ] }
  ],
  "pinned": [],
  "active": { "global": 1 }
}"#;

    fn round_trip(cx: &Contexts) -> Contexts {
        serde_json::from_str(&serde_json::to_string(cx).unwrap()).unwrap()
    }

    #[test]
    fn contexts_json_round_trips_the_spec_example() {
        let cx: Contexts = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        let comms = cx.by_name("Comms").unwrap();
        assert_eq!(comms.id, ContextId(1));
        assert_eq!(comms.number, Some(1));
        assert_eq!(
            comms.members,
            vec![MemberRecord {
                bundle_id: Some("net.whatsapp.WhatsApp".into()),
                app_name: Some("WhatsApp".into()),
                title: "WhatsApp".into(),
                window_server_id: Some(WindowServerId(81234)),
                link: RecordLink::Empty,
            }]
        );
        assert_eq!(cx.active(), ContextKey::Named(ContextId(1)));
        assert_eq!(cx.next_id, 3);
        assert_eq!(cx.use_seq, 42);
        let written = serde_json::to_value(&cx).unwrap();
        let expected: serde_json::Value = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        assert_eq!(written, expected);
    }

    #[test]
    fn contexts_json_writes_built_in_active_contexts_as_strings() {
        let mut cx = Contexts::new();
        assert_eq!(
            serde_json::to_value(&cx).unwrap()["active"],
            serde_json::json!({ "global": "everything" })
        );
        cx.switch_to(ContextKey::Unsorted).unwrap();
        assert_eq!(
            serde_json::to_value(&cx).unwrap()["active"],
            serde_json::json!({ "global": "unsorted" })
        );
        assert_eq!(round_trip(&cx).active(), ContextKey::Unsorted);
    }

    #[test]
    fn r28_contexts_json_without_a_known_active_context_loads_as_everything() {
        let mut doc: serde_json::Value = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        for active in [
            serde_json::Value::Null,
            serde_json::json!({ "global": 7 }),
            serde_json::json!({ "global": "somewhere" }),
            serde_json::json!({ "per_screen": { "1": 1 } }),
        ] {
            doc["active"] = active;
            let cx: Contexts = serde_json::from_value(doc.clone()).unwrap();
            assert_eq!(cx.active(), ContextKey::Everything);
        }
        doc.as_object_mut().unwrap().remove("active");
        let cx: Contexts = serde_json::from_value(doc).unwrap();
        assert_eq!(cx.active(), ContextKey::Everything);
    }

    /// R8. `contexts.json` holds each screen's active context next to the
    /// global one, and a restart restores them. A screen with no entry shows
    /// Everything.
    #[test]
    fn m9_contexts_json_holds_the_active_context_of_each_screen() {
        let mut cx = Contexts::new();
        let comms = cx.create("Comms").unwrap();
        let build = cx.create("Build").unwrap();
        cx.switch_to(ContextKey::Named(comms)).unwrap();
        cx.switch_to_on(ScreenId::new(1), ContextKey::Named(comms)).unwrap();
        cx.switch_to_on(ScreenId::new(2), ContextKey::Named(build)).unwrap();

        let written = serde_json::to_value(&cx).unwrap();
        assert_eq!(
            written["active"],
            serde_json::json!({ "per_screen": { "1": 1, "2": 2 } })
        );

        let restored = round_trip(&cx);
        assert_eq!(restored.active(), ContextKey::Everything);
        assert_eq!(restored.active_on(ScreenId::new(1)), ContextKey::Named(comms));
        assert_eq!(restored.active_on(ScreenId::new(2)), ContextKey::Named(build));
        assert_eq!(restored.active_on(ScreenId::new(3)), ContextKey::Everything);
        // The global value alone keeps the old shape.
        let mut global = Contexts::new();
        global.switch_to(ContextKey::Unsorted).unwrap();
        assert_eq!(
            serde_json::to_value(&global).unwrap()["active"],
            serde_json::json!({ "global": "unsorted" })
        );
    }

    /// R8. A screen's saved context that no longer exists, and a saved value of
    /// another shape, load as Everything. Built-ins load as themselves.
    #[test]
    fn m9_a_screens_saved_context_that_is_gone_loads_as_everything() {
        let doc = serde_json::json!({
            "version": 1,
            "next_id": 2,
            "contexts": [{ "id": 1, "name": "A" }],
            "active": {
                "global": 1,
                "per_screen": { "1": 9, "2": "unsorted", "3": "everything", "4": "nonsense" }
            }
        });

        let cx: Contexts = serde_json::from_value(doc).unwrap();

        assert_eq!(cx.active_on(ScreenId::new(1)), ContextKey::Everything);
        assert_eq!(cx.active_on(ScreenId::new(2)), ContextKey::Unsorted);
        assert_eq!(cx.active_on(ScreenId::new(3)), ContextKey::Everything);
        assert_eq!(cx.active_on(ScreenId::new(4)), ContextKey::Everything);
        assert_eq!(cx.active_on(ScreenId::new(5)), ContextKey::Everything);
    }

    /// R6. Deleting the context a screen shows makes that screen show
    /// Unsorted, which counts as a use of it. Other screens are untouched.
    #[test]
    fn r6_deleting_the_active_context_of_a_screen_shows_unsorted_there() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let left = ScreenId::new(1);
        cx.switch_to_on(left, ContextKey::Named(a)).unwrap();
        cx.switch_to_on(ScreenId::new(2), ContextKey::Named(b)).unwrap();
        let before = cx.last_used(ContextKey::Unsorted);

        cx.delete(a).unwrap();

        assert_eq!(cx.active_on(left), ContextKey::Unsorted);
        assert_eq!(cx.active_on(ScreenId::new(2)), ContextKey::Named(b));
        assert!(cx.last_used(ContextKey::Unsorted) > before);
        assert_eq!(cx.active(), ContextKey::Everything);
        // The screen's first switch was from Everything.
        assert_eq!(cx.previous_on(left), Some(ContextKey::Everything));
    }

    /// R18. Each screen remembers the context it used before its active one.
    #[test]
    fn r18_previous_contexts_are_per_screen() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let left = ScreenId::new(1);
        cx.switch_to_on(left, ContextKey::Named(a)).unwrap();
        cx.switch_to_on(left, ContextKey::Named(b)).unwrap();
        // A switch to the active context again leaves the previous alone.
        cx.switch_to_on(left, ContextKey::Named(b)).unwrap();

        assert_eq!(cx.previous_on(left), Some(ContextKey::Named(a)));
        assert_eq!(cx.previous_on(ScreenId::new(2)), None);
        assert_eq!(cx.previous(), None);
    }

    /// R11. Going from `global` to `per_screen` gives every screen the
    /// global context without counting a use. Going back forgets them.
    #[test]
    fn r11_screen_actives_are_adopted_and_forgotten() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        cx.switch_to(ContextKey::Named(a)).unwrap();
        let used = cx.last_used(ContextKey::Named(a));

        cx.set_screen_actives([ScreenId::new(1), ScreenId::new(2)], ContextKey::Named(a));

        assert_eq!(cx.active_on(ScreenId::new(1)), ContextKey::Named(a));
        assert_eq!(cx.active_on(ScreenId::new(2)), ContextKey::Named(a));
        assert_eq!(cx.active_on(ScreenId::new(3)), ContextKey::Everything);
        assert_eq!(used, cx.last_used(ContextKey::Named(a)));
        cx.forget_screen_actives();
        assert_eq!(cx.active_on(ScreenId::new(1)), ContextKey::Everything);
        assert_eq!(cx.active(), ContextKey::Named(a));
    }

    #[test]
    fn contexts_json_rejects_other_versions() {
        let mut doc: serde_json::Value = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        doc["version"] = serde_json::json!(2);
        assert!(serde_json::from_value::<Contexts>(doc).is_err());
    }

    #[test]
    fn contexts_json_repairs_ids_and_numbers() {
        let doc = serde_json::json!({
            "version": 1,
            "next_id": 1,
            "use_seq": 0,
            "contexts": [
                { "id": 4, "name": "A", "number": 2, "last_used": 9, "members": [] },
                { "id": 5, "name": "B", "number": 2, "last_used": 0, "members": [] },
                { "id": 6, "name": "C", "number": 12, "last_used": 0, "members": [] },
                { "id": 6, "name": "Duplicate", "number": null, "last_used": 0,
                  "members": [{ "bundle_id": "com.example.App", "title": "Kept" }] }
            ],
            "pinned": []
        });
        let mut cx: Contexts = serde_json::from_value(doc).unwrap();
        let numbers: Vec<_> = cx.contexts().iter().map(|c| c.number).collect();
        assert_eq!(numbers, vec![Some(2), None, None, None]);
        // The duplicate id gets a fresh id and keeps its members.
        let duplicate = cx.by_name("Duplicate").unwrap();
        assert_eq!(duplicate.id, ContextId(7));
        assert_eq!(records(&cx, ContextId(7)), vec![("Kept", RecordLink::Empty)]);
        assert_eq!(cx.get(ContextId(6)).unwrap().name, "C");
        assert_eq!(cx.create("D").unwrap(), ContextId(8));
        cx.switch_to(ContextKey::Named(ContextId(5))).unwrap();
        assert!(
            cx.last_used(ContextKey::Named(ContextId(5)))
                > cx.last_used(ContextKey::Named(ContextId(4)))
        );
    }

    #[test]
    fn r23_records_stay_across_a_restart_without_their_live_windows() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let open = window(1, 1, "App", "Open");
        let closed = window(1, 2, "App", "Closed");
        cx.add_window(a, &open).unwrap();
        cx.add_window(a, &closed).unwrap();
        cx.pin(&open);
        cx.window_closed(closed.wid);
        let written = serde_json::to_value(&cx).unwrap();
        assert_eq!(
            written["contexts"][0]["members"][0].as_object().unwrap().len(),
            4
        );
        let restored = round_trip(&cx);
        let members = &restored.get(a).unwrap().members;
        assert_eq!(
            members.iter().map(|m| m.title.as_str()).collect::<Vec<_>>(),
            vec!["Open", "Closed"]
        );
        assert!(members.iter().all(|m| m.link == RecordLink::Empty));
        assert_eq!(restored.pinned()[0].link, RecordLink::Empty);
        assert!(restored.contexts_of(open.wid).is_empty());
    }

    fn context_names(cx: &Contexts) -> Vec<&str> {
        cx.contexts().iter().map(|c| c.name.as_str()).collect()
    }

    fn records(cx: &Contexts, id: ContextId) -> Vec<(&str, RecordLink)> {
        cx.get(id).unwrap().members.iter().map(|m| (m.title.as_str(), m.link)).collect()
    }

    fn exact_title(slot: Slot, index: usize) -> RecordMatch {
        RecordMatch {
            slot,
            index,
            step: MatchStep::ExactTitle,
        }
    }

    /// R1: a context holds one window of an app, not the app's other windows.
    #[test]
    fn r1_a_context_holds_windows_not_apps() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let docs = window(1, 1, "Chrome", "Docs");
        let mail = window(1, 2, "Chrome", "Mail");
        assert!(cx.add_window(a, &docs).unwrap());
        assert_eq!(
            cx.get(a).unwrap().members,
            vec![MemberRecord {
                bundle_id: Some("com.example.Chrome".into()),
                app_name: Some("Chrome".into()),
                title: "Docs".into(),
                window_server_id: Some(WindowServerId(1001)),
                link: RecordLink::Live(docs.wid),
            }]
        );
        assert_eq!(cx.contexts_of(mail.wid), vec![]);
        assert!(cx.is_unsorted(mail.wid));
        cx.switch_to(named(a)).unwrap();
        let plan = plan_switch(&one_screen(
            cx.active(),
            vec![cx.switch_window(docs.wid), cx.switch_window(mail.wid)],
        ));
        assert_eq!(plan.park, vec![mail.wid]);
    }

    /// R3: a pinned window shows in a context created after it was pinned,
    /// and focusing it there never switches.
    #[test]
    fn r3_a_context_created_after_pinning_shows_the_pinned_windows() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let music = window(1, 1, "Music", "Music");
        let timer = window(2, 1, "Timer", "Timer");
        let mail = window(3, 1, "Mail", "Inbox");
        cx.add_window(a, &mail).unwrap();
        cx.pin(&music);
        cx.pin(&timer);
        let later = cx.create("Later").unwrap();
        cx.switch_to(named(later)).unwrap();
        let plan = plan_switch(&one_screen(
            cx.active(),
            vec![
                cx.switch_window(music.wid),
                SwitchWindow {
                    parked: true,
                    ..cx.switch_window(timer.wid)
                },
                cx.switch_window(mail.wid),
            ],
        ));
        assert_eq!(plan.park, vec![mail.wid]);
        assert_eq!(plan.unpark, vec![timer.wid]);
        assert_eq!(cx.focus_target(music.wid), named(later));
        assert_eq!(cx.contexts_of(music.wid), vec![]);
        assert!(!cx.is_unsorted(music.wid));
    }

    /// R3: a pinned window that rejoins after its app relaunched is a member
    /// of contexts created after the relaunch.
    #[test]
    fn r3_a_rejoined_pinned_window_is_in_contexts_created_later() {
        let mut cx = Contexts::new();
        cx.pin(&window(1, 1, "Music", "Music"));
        cx.app_terminated(1);
        let relaunched = window(2, 1, "Music", "Music");
        assert_eq!(
            cx.window_appeared(&relaunched, ContextKey::Everything),
            Arrival::Rejoined(vec![exact_title(Slot::Pinned, 0)])
        );
        let later = cx.create("Later").unwrap();
        assert!(cx.is_member(named(later), relaunched.wid));
        assert!(cx.is_member(ContextKey::Unsorted, relaunched.wid));
        assert!(!cx.is_unsorted(relaunched.wid));
    }

    /// R4: the reserved names are refused in any case and with surrounding
    /// spaces, for new contexts and for renames.
    #[test]
    fn r4_reserved_names_are_refused_in_any_case() {
        let mut cx = Contexts::new();
        for (name, reported) in [
            ("EVERYTHING", "EVERYTHING"),
            (" everything ", "everything"),
            ("uNsOrTeD", "uNsOrTeD"),
        ] {
            assert_eq!(cx.create(name), Err(ContextError::ReservedName(reported.into())));
        }
        let a = cx.create("A").unwrap();
        assert_eq!(
            cx.rename(a, "Everything"),
            Err(ContextError::ReservedName("Everything".into()))
        );
        assert_eq!(context_names(&cx), vec!["A"]);
    }

    /// R4: names are unique ignoring case, accented letters included, and a
    /// deleted context's name is free again.
    #[test]
    fn r4_case_variants_are_refused_until_the_name_is_free() {
        let mut cx = Contexts::new();
        assert_eq!(cx.create(""), Err(ContextError::EmptyName));
        let cafe = cx.create("  Café  ").unwrap();
        assert_eq!(cx.get(cafe).unwrap().name, "Café");
        for variant in ["café", "CAFÉ", " cAfÉ "] {
            assert_eq!(cx.create(variant), Err(ContextError::NameTaken("Café".into())));
        }
        let other = cx.create("Other").unwrap();
        assert_eq!(
            cx.rename(other, "CAFÉ"),
            Err(ContextError::NameTaken("Café".into()))
        );
        assert_eq!(cx.rename(other, " "), Err(ContextError::EmptyName));
        assert_eq!(
            cx.rename(ContextId(99), "Fresh"),
            Err(ContextError::NoSuchContext)
        );
        assert_eq!(context_names(&cx), vec!["Café", "Other"]);
        cx.delete(cafe).unwrap();
        let again = cx.create("CAFÉ").unwrap();
        assert_eq!(context_names(&cx), vec!["Other", "CAFÉ"]);
        assert_ne!(again, cafe);
    }

    /// R4 holds for a loaded `contexts.json`: no loaded name is empty,
    /// reserved, or a case variant of another. Refusing the file is also
    /// acceptable.
    #[test]
    fn r4_loaded_names_are_unique_and_not_reserved() {
        let doc = serde_json::json!({
            "version": 1,
            "next_id": 6,
            "use_seq": 0,
            "contexts": [
                { "id": 1, "name": "Comms" },
                { "id": 2, "name": "comms" },
                { "id": 3, "name": "Everything" },
                { "id": 4, "name": " unsorted " },
                { "id": 5, "name": " " }
            ]
        });
        let Ok(cx) = serde_json::from_value::<Contexts>(doc) else {
            return;
        };
        let mut seen: Vec<String> = Vec::new();
        for context in cx.contexts() {
            let name = context.name.trim().to_lowercase();
            assert!(!name.is_empty(), "empty name {:?}", context.name);
            assert!(
                name != "everything" && name != "unsorted",
                "reserved name {:?}",
                context.name
            );
            assert!(!seen.contains(&name), "repeated name {:?}", context.name);
            seen.push(name);
        }
    }

    /// R5: a new context takes the lowest number that renumbering left
    /// free, and a stolen number leaves its old owner without one.
    #[test]
    fn r5_new_contexts_take_numbers_freed_by_renumbering() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        cx.set_number(a, Some(5)).unwrap();
        cx.set_number(b, Some(2)).unwrap();
        let c = cx.create("C").unwrap();
        let d = cx.create("D").unwrap();
        cx.set_number(c, Some(5)).unwrap();
        let e = cx.create("E").unwrap();
        assert_eq!(
            [a, b, c, d, e].map(|id| cx.get(id).unwrap().number),
            [None, Some(2), Some(5), Some(3), Some(1)]
        );
    }

    /// R5: a loaded number outside 1 to 9, 0 included, is dropped, and a new
    /// context skips the loaded numbers.
    #[test]
    fn r5_loaded_numbers_outside_one_to_nine_are_dropped() {
        let doc = serde_json::json!({
            "version": 1,
            "next_id": 4,
            "use_seq": 0,
            "contexts": [
                { "id": 1, "name": "A", "number": 0 },
                { "id": 2, "name": "B", "number": 1 },
                { "id": 3, "name": "C", "number": 9 }
            ]
        });
        let mut cx: Contexts = serde_json::from_value(doc).unwrap();
        let d = cx.create("D").unwrap();
        assert_eq!(d, ContextId(4));
        assert_eq!(
            cx.contexts().iter().map(|c| c.number).collect::<Vec<_>>(),
            vec![None, Some(1), Some(9), Some(2)]
        );
    }

    /// R6: after the active context is deleted, Unsorted is active. A switch
    /// then keeps the windows that were only in the deleted context, parks
    /// the ones in another context, and puts back the unsorted ones.
    #[test]
    fn r6_deleting_the_active_context_keeps_its_only_members_visible() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let only_a = window(1, 1, "App", "Only A");
        let shared = window(1, 2, "App", "Shared");
        let only_b = window(2, 1, "Other", "Only B");
        let loose = window(3, 1, "Loose", "Loose");
        let music = window(4, 1, "Music", "Music");
        cx.add_window(a, &only_a).unwrap();
        cx.add_window(a, &shared).unwrap();
        cx.add_window(b, &shared).unwrap();
        cx.add_window(b, &only_b).unwrap();
        cx.pin(&music);
        cx.switch_to(named(a)).unwrap();
        assert_eq!(cx.delete(a).unwrap().name, "A");
        assert_eq!(cx.active(), ContextKey::Unsorted);
        assert_eq!(context_names(&cx), vec!["B"]);
        assert_eq!(
            records(&cx, b),
            vec![
                ("Shared", RecordLink::Live(shared.wid)),
                ("Only B", RecordLink::Live(only_b.wid)),
            ]
        );
        // Under A, only_b and loose were parked.
        let plan = plan_switch(&one_screen(
            cx.active(),
            vec![
                cx.switch_window(only_a.wid),
                cx.switch_window(shared.wid),
                SwitchWindow {
                    parked: true,
                    ..cx.switch_window(only_b.wid)
                },
                SwitchWindow {
                    parked: true,
                    ..cx.switch_window(loose.wid)
                },
                cx.switch_window(music.wid),
            ],
        ));
        assert_eq!(plan.park, vec![shared.wid]);
        assert_eq!(plan.unpark, vec![loose.wid]);
    }

    /// R6, R18: deleting the active context never leaves the previous
    /// context equal to the new active one.
    #[test]
    fn r6_deleting_the_active_context_never_makes_it_previous() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        cx.switch_to(ContextKey::Unsorted).unwrap();
        cx.switch_to(named(a)).unwrap();
        cx.delete(a).unwrap();
        assert_eq!(cx.active(), ContextKey::Unsorted);
        assert_ne!(cx.previous(), Some(ContextKey::Unsorted));
    }

    /// R6, R18, R19: deleting the active context counts as a use of
    /// Unsorted, and the context used before it stays the previous one.
    #[test]
    fn r6_deleting_the_active_context_uses_unsorted_and_keeps_previous() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        cx.switch_to(named(b)).unwrap();
        cx.switch_to(named(a)).unwrap();
        cx.delete(a).unwrap();
        assert_eq!(
            (cx.active(), cx.previous()),
            (ContextKey::Unsorted, Some(named(b)))
        );
        assert_eq!(cx.last_used(ContextKey::Unsorted), 3);
        assert_eq!(ranked_names("", &cx, true), vec!["Unsorted", "B", "Everything"]);
        cx.switch_to(cx.previous().unwrap()).unwrap();
        assert_eq!(
            (cx.active(), cx.previous()),
            (named(b), Some(ContextKey::Unsorted))
        );
    }

    /// R10: a window on a Space nobody sees is left alone. When its Space
    /// becomes visible, the same active context applies to it.
    #[test]
    fn r10_windows_are_planned_when_their_space_becomes_visible() {
        let stray = member_of(wid(1, 1), &[B]);
        let plan = plan_switch(&one_screen(
            ContextKey::Named(A),
            vec![SwitchWindow {
                invisible: true,
                ..stray.clone()
            }],
        ));
        assert_eq!(plan, SwitchPlan::default());
        let parked_member = SwitchWindow {
            parked: true,
            ..member_of(wid(1, 2), &[A])
        };
        let plan = plan_switch(&one_screen(ContextKey::Named(A), vec![stray, parked_member]));
        assert_eq!(
            plan,
            SwitchPlan {
                moves: vec![],
                park: vec![wid(1, 1)],
                unpark: vec![wid(1, 2)],
                focus: Some(wid(1, 2)),
            }
        );
    }

    /// R12: with no screens or no windows, a switch does nothing.
    #[test]
    fn r12_a_switch_without_windows_plans_nothing() {
        assert_eq!(
            plan_switch(&SwitchInput { screens: vec![], only: None }),
            SwitchPlan::default()
        );
        for active in [
            ContextKey::Everything,
            ContextKey::Unsorted,
            ContextKey::Named(A),
        ] {
            assert_eq!(plan_switch(&one_screen(active, vec![])), SwitchPlan::default());
        }
    }

    /// R7, R12: in global scope the focus goes to the most recently focused
    /// member on any screen, including a member that is put back.
    #[test]
    fn r12_focus_is_the_most_recently_focused_member_on_any_screen() {
        let input = SwitchInput {
            only: None,
            screens: vec![
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![
                        SwitchWindow {
                            last_focus: Some(4),
                            ..member_of(wid(1, 1), &[A])
                        },
                        SwitchWindow {
                            last_focus: Some(9),
                            ..member_of(wid(1, 2), &[B])
                        },
                    ],
                },
                SwitchScreen {
                    active: ContextKey::Named(A),
                    windows: vec![
                        SwitchWindow {
                            last_focus: Some(7),
                            parked: true,
                            ..member_of(wid(2, 1), &[A, B])
                        },
                        SwitchWindow {
                            last_focus: Some(6),
                            pinned: true,
                            ..member_of(wid(3, 1), &[])
                        },
                    ],
                },
            ],
        };
        assert_eq!(
            plan_switch(&input),
            SwitchPlan {
                moves: vec![],
                park: vec![wid(1, 2)],
                unpark: vec![wid(2, 1)],
                focus: Some(wid(2, 1)),
            }
        );
    }

    /// R3, R13: a switch shows exactly the members of the active context and
    /// parks every other window. Pinned windows show under Unsorted too.
    #[test]
    fn r13_a_switch_shows_exactly_the_members_of_each_context() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let in_a = window(1, 1, "App", "In A");
        let in_b = window(1, 2, "App", "In B");
        let in_both = window(1, 3, "App", "In both");
        let pinned = window(1, 4, "App", "Pinned");
        let pinned_in_a = window(1, 5, "App", "Pinned in A");
        let loose = window(1, 6, "App", "Loose");
        for (id, w) in [
            (a, &in_a),
            (b, &in_b),
            (a, &in_both),
            (b, &in_both),
            (a, &pinned_in_a),
        ] {
            cx.add_window(id, w).unwrap();
        }
        cx.pin(&pinned);
        cx.pin(&pinned_in_a);
        let all = [&in_a, &in_b, &in_both, &pinned, &pinned_in_a, &loose].map(|w| w.wid);
        for (active, shown) in [
            (ContextKey::Everything, all.to_vec()),
            (
                ContextKey::Unsorted,
                vec![pinned.wid, pinned_in_a.wid, loose.wid],
            ),
            (
                named(a),
                vec![in_a.wid, in_both.wid, pinned.wid, pinned_in_a.wid],
            ),
            (
                named(b),
                vec![in_b.wid, in_both.wid, pinned.wid, pinned_in_a.wid],
            ),
        ] {
            let plan = plan_switch(&one_screen(
                active,
                all.map(|wid| cx.switch_window(wid)).to_vec(),
            ));
            let hidden: Vec<WindowId> =
                all.into_iter().filter(|wid| !shown.contains(wid)).collect();
            assert_eq!(plan.park, hidden, "{active:?}");
            let members: Vec<WindowId> =
                all.into_iter().filter(|wid| cx.is_member(active, *wid)).collect();
            assert_eq!(members, shown, "{active:?}");
        }
    }

    /// R14 holds under Unsorted and every named context.
    #[test]
    fn r14_never_parks_protected_windows_under_any_context() {
        let windows = vec![
            SwitchWindow {
                own: true,
                ..member_of(wid(1, 1), &[A])
            },
            SwitchWindow {
                untracked: true,
                ..member_of(wid(1, 2), &[A])
            },
            SwitchWindow {
                invisible: true,
                ..member_of(wid(1, 3), &[A])
            },
        ];
        for active in [ContextKey::Unsorted, ContextKey::Named(B)] {
            let plan = plan_switch(&one_screen(active, windows.clone()));
            assert_eq!(plan, SwitchPlan::default(), "{active:?}");
        }
    }

    /// R16, R19: every switch, including one to the active context, takes
    /// the next use number. A failed switch takes none.
    #[test]
    fn r19_every_switch_takes_the_next_use_number() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        cx.switch_to(named(a)).unwrap();
        cx.switch_to(named(b)).unwrap();
        cx.switch_to(named(b)).unwrap();
        cx.switch_to(ContextKey::Everything).unwrap();
        cx.switch_to(ContextKey::Unsorted).unwrap();
        assert!(cx.switch_to(named(ContextId(99))).is_err());
        assert_eq!(
            [
                named(a),
                named(b),
                ContextKey::Everything,
                ContextKey::Unsorted
            ]
            .map(|key| cx.last_used(key)),
            [1, 3, 4, 5]
        );
        assert_eq!(serde_json::to_value(&cx).unwrap()["use_seq"], 5);
    }

    /// R18: switching to the previous context again and again toggles
    /// between the last two.
    #[test]
    fn r18_previous_context_toggles_between_the_last_two() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        cx.switch_to(named(a)).unwrap();
        cx.switch_to(named(b)).unwrap();
        for (active, previous) in [(a, b), (b, a), (a, b)] {
            cx.switch_to(cx.previous().unwrap()).unwrap();
            assert_eq!(
                (cx.active(), cx.previous()),
                (named(active), Some(named(previous)))
            );
        }
    }

    /// R20, R23: a new window that only matches a pending record joins the
    /// active context. The pending record goes once the app shows it is
    /// still running.
    #[test]
    fn r20_a_new_window_passes_over_pending_records() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let closed = window(1, 1, "Mail", "Inbox");
        cx.add_window(a, &closed).unwrap();
        cx.switch_to(named(b)).unwrap();
        cx.window_closed(closed.wid);
        let fresh = window(1, 2, "Mail", "Inbox");
        assert_eq!(cx.window_appeared(&fresh, cx.active()), Arrival::Joined(b));
        assert_eq!(records(&cx, a), vec![("Inbox", RecordLink::Pending(closed.wid))]);
        cx.app_still_running(1);
        assert_eq!(records(&cx, a), vec![]);
        assert_eq!(records(&cx, b), vec![("Inbox", RecordLink::Live(fresh.wid))]);
    }

    /// R20: a live record never takes a second window with the same title.
    #[test]
    fn r20_a_second_window_with_a_members_title_joins_the_active_context() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let first = window(1, 1, "Terminal", "~/src");
        cx.add_window(a, &first).unwrap();
        cx.switch_to(named(b)).unwrap();
        let second = window(1, 2, "Terminal", "~/src");
        assert_eq!(cx.window_appeared(&second, cx.active()), Arrival::Joined(b));
        assert_eq!(cx.contexts_of(first.wid), vec![a]);
        assert_eq!(cx.contexts_of(second.wid), vec![b]);
    }

    /// R20, R22: when a window appears, step 4 doesn't claim it for a record
    /// of its app with another title. It joins the active context and shows.
    #[test]
    fn r22_step_4_never_runs_when_a_window_appears() {
        let (mut cx, ids) = with_records(&[
            ("A", vec![empty_record("Chrome", "Docs", None)]),
            ("B", vec![]),
        ]);
        cx.switch_to(named(ids[1])).unwrap();
        let gmail = window(1, 1, "Chrome", "Gmail");
        assert_eq!(cx.window_appeared(&gmail, cx.active()), Arrival::Joined(ids[1]));
        assert_eq!(
            cx.get(ids[0]).unwrap().members,
            vec![empty_record("Chrome", "Docs", None)]
        );
        let plan = plan_switch(&one_screen(cx.active(), vec![cx.switch_window(gmail.wid)]));
        assert_eq!(plan.park, vec![]);
    }

    /// R21: a relaunched window rejoins exactly the contexts that hold its
    /// record, the records take its new details, and it shows while one of
    /// those contexts is active.
    #[test]
    fn r21_a_rejoined_window_shows_only_under_its_contexts() {
        let mut cx = Contexts::new();
        let comms = cx.create("Comms").unwrap();
        let relax = cx.create("Relax").unwrap();
        let work = cx.create("Work").unwrap();
        let whatsapp = window(1, 1, "WhatsApp", "WhatsApp");
        cx.add_window(comms, &whatsapp).unwrap();
        cx.add_window(relax, &whatsapp).unwrap();
        cx.app_terminated(1);
        cx.switch_to(named(relax)).unwrap();
        let relaunched = window(2, 1, "WhatsApp", "WhatsApp");
        assert_eq!(
            cx.window_appeared(&relaunched, cx.active()),
            Arrival::Rejoined(vec![
                exact_title(Slot::Context(comms), 0),
                exact_title(Slot::Context(relax), 0),
            ])
        );
        let record = MemberRecord {
            bundle_id: Some("com.example.WhatsApp".into()),
            app_name: Some("WhatsApp".into()),
            title: "WhatsApp".into(),
            window_server_id: Some(WindowServerId(2001)),
            link: RecordLink::Live(relaunched.wid),
        };
        assert_eq!(cx.get(comms).unwrap().members, vec![record.clone()]);
        assert_eq!(cx.get(relax).unwrap().members, vec![record]);
        assert_eq!(cx.get(work).unwrap().members, vec![]);
        let plan = plan_switch(&one_screen(cx.active(), vec![cx.switch_window(relaunched.wid)]));
        assert_eq!(plan.park, vec![]);
        cx.switch_to(named(work)).unwrap();
        let plan = plan_switch(&one_screen(cx.active(), vec![cx.switch_window(relaunched.wid)]));
        assert_eq!(plan.park, vec![relaunched.wid]);
    }

    /// R22 step 1 needs the same app as well as the same window server id.
    #[test]
    fn r22_step_1_requires_the_same_app() {
        let (cx, ids) =
            with_records(&[("A", vec![empty_record("Other", "Unrelated", Some(1001))])]);
        let w = window(1, 1, "App", "Title");
        assert_eq!(
            match_window(&w, &cx, MatchPass::Switch { target: named(ids[0]) }),
            vec![]
        );
    }

    /// R22: within one context, a window server id match wins over an exact
    /// title that comes first in the list.
    #[test]
    fn r22_step_1_beats_step_2_within_one_context() {
        let (cx, ids) = with_records(&[(
            "A",
            vec![
                empty_record("App", "Title", None),
                empty_record("App", "Old title", Some(1001)),
            ],
        )]);
        let w = window(1, 1, "App", "Title");
        assert_eq!(
            match_window(&w, &cx, MatchPass::Arrival),
            vec![RecordMatch {
                slot: Slot::Context(ids[0]),
                index: 1,
                step: MatchStep::WindowServerId,
            }]
        );
    }

    /// R21, R22: steps 1 and 2 have no "in no other context" condition, so a
    /// window can match at step 1 in one context and at step 2 in another.
    #[test]
    fn r22_steps_1_and_2_match_in_different_contexts() {
        let (cx, ids) = with_records(&[
            ("A", vec![empty_record("App", "Old title", Some(1001))]),
            ("B", vec![empty_record("App", "Title", None)]),
        ]);
        let w = window(1, 1, "App", "Title");
        assert_eq!(
            match_window(&w, &cx, MatchPass::Arrival),
            vec![
                RecordMatch {
                    slot: Slot::Context(ids[0]),
                    index: 0,
                    step: MatchStep::WindowServerId,
                },
                exact_title(Slot::Context(ids[1]), 0),
            ]
        );
    }

    /// R22 step 2 needs exactly the same title. A title that differs only in
    /// case matches at step 3.
    #[test]
    fn r22_step_2_is_case_sensitive() {
        let (cx, ids) = with_records(&[("A", vec![empty_record("App", "Inbox", None)])]);
        let w = window(1, 1, "App", "INBOX");
        assert_eq!(
            steps(&match_window(&w, &cx, MatchPass::Arrival)),
            vec![(Slot::Context(ids[0]), MatchStep::SimilarTitle)]
        );
    }

    /// R22: the same app means the same bundle id, even when two apps share
    /// a name.
    #[test]
    fn r22_apps_that_share_a_name_but_not_a_bundle_id_never_match() {
        let record = MemberRecord {
            bundle_id: Some("com.google.Chrome".into()),
            app_name: Some("Google Chrome".into()),
            title: "Docs".into(),
            window_server_id: None,
            link: RecordLink::Empty,
        };
        let (cx, ids) = with_records(&[("A", vec![record])]);
        let beta = WindowDesc {
            wid: wid(1, 1),
            bundle_id: Some("com.google.Chrome.beta".into()),
            app_name: Some("Google Chrome".into()),
            title: "Docs".into(),
            window_server_id: None,
        };
        assert_eq!(
            match_window(&beta, &cx, MatchPass::Switch { target: named(ids[0]) }),
            vec![]
        );
    }

    /// R22: an empty title never matches at step 2. Otherwise a new untitled
    /// window would rejoin an old record and be parked (R21), the harm that
    /// keeps step 4 out of arrivals. Rooms' exact-title pass also needs a
    /// title.
    #[test]
    fn r22_an_empty_title_never_matches_at_step_2() {
        let (mut cx, ids) =
            with_records(&[("A", vec![empty_record("App", "", None)]), ("B", vec![])]);
        cx.switch_to(named(ids[1])).unwrap();
        let untitled = window(1, 1, "App", "");
        assert_eq!(
            cx.window_appeared(&untitled, cx.active()),
            Arrival::Joined(ids[1])
        );
        assert_eq!(records(&cx, ids[0]), vec![("", RecordLink::Empty)]);
    }

    /// R22: each record binds one window, so two relaunched windows with one
    /// title take two records, and a third window joins nothing.
    #[test]
    fn r22_two_windows_with_one_title_take_two_records() {
        let (mut cx, ids) = with_records(&[(
            "A",
            vec![
                empty_record("Chrome", "New Tab", None),
                empty_record("Chrome", "New Tab", None),
            ],
        )]);
        let first = window(1, 1, "Chrome", "New Tab");
        let second = window(1, 2, "Chrome", "New Tab");
        let third = window(1, 3, "Chrome", "New Tab");
        let slot = Slot::Context(ids[0]);
        assert_eq!(
            cx.window_appeared(&first, ContextKey::Everything),
            Arrival::Rejoined(vec![exact_title(slot, 0)])
        );
        assert_eq!(
            cx.window_appeared(&second, ContextKey::Everything),
            Arrival::Rejoined(vec![exact_title(slot, 1)])
        );
        assert_eq!(
            cx.window_appeared(&third, ContextKey::Everything),
            Arrival::Unsorted
        );
        assert_eq!(
            records(&cx, ids[0]),
            vec![
                ("New Tab", RecordLink::Live(first.wid)),
                ("New Tab", RecordLink::Live(second.wid)),
            ]
        );
    }

    /// R22 step 3, story 11: a window whose title changed a little while its
    /// app was closed rejoins every context that holds its record.
    #[test]
    fn r22_step_3_rejoins_every_context_with_a_similar_record() {
        let (mut cx, ids) = with_records(&[
            ("Comms", vec![empty_record("WhatsApp", "WhatsApp", None)]),
            ("Relax", vec![empty_record("WhatsApp", "WhatsApp", None)]),
        ]);
        let w = window(1, 1, "WhatsApp", "WhatsApp (3)");
        assert_eq!(
            steps(&cx.rejoin(&w, MatchPass::Arrival)),
            vec![
                (Slot::Context(ids[0]), MatchStep::SimilarTitle),
                (Slot::Context(ids[1]), MatchStep::SimilarTitle),
            ]
        );
        assert_eq!(
            records(&cx, ids[1]),
            vec![("WhatsApp (3)", RecordLink::Live(w.wid))]
        );
    }

    /// R22: both titles need at least 4 characters after folding. Characters
    /// are counted, not bytes, and combining accents don't count.
    #[test]
    fn r22_similar_titles_need_four_folded_characters_each() {
        assert!(similar_titles("Mail", "Gmail"));
        assert!(similar_titles("Mail", "MAIL"));
        assert!(!similar_titles("Mai", "Mail"));
        assert!(!similar_titles("Mai", "Main menu"));
        assert!(!similar_titles("", ""));
        assert!(!similar_titles("", "Mail"));
        // 4 characters in 5 bytes.
        assert!(similar_titles("Café", "Cafe society"));
        // 3 characters in 9 bytes, then 4 in 12.
        assert!(!similar_titles("日本語", "日本語の本"));
        assert!(similar_titles("日本語の", "日本語の本"));
        // 4 characters, one of them a combining accent.
        assert!(!similar_titles("Moe\u{301}", "Moe\u{301} and more"));
        assert!(similar_titles("Mote\u{301}", "mote and more"));
    }

    /// R22: without containment, a shared prefix of min(12, two-thirds of
    /// the shorter title) is enough. These shorter titles have lengths that
    /// are multiples of 3, so two-thirds is a whole number.
    #[test]
    fn r22_similar_prefix_threshold_at_whole_two_thirds() {
        // Shorter title 6: 4 characters.
        assert!(similar_titles("Report", "Repo-99"));
        assert!(!similar_titles("Report", "Rep-999"));
        // Shorter title 12: 8 characters.
        assert!(similar_titles("Budget 2026a", "Budget 2-xxxxx"));
        assert!(!similar_titles("Budget 2026a", "Budget -xxxxxx"));
        // Shorter title 18: 12 characters.
        assert!(similar_titles("Quarterly report 1", "Quarterly re-xxxxxxx"));
        assert!(!similar_titles("Quarterly report 1", "Quarterly r-xxxxxxxx"));
        // Shorter title 21: the cap of 12 applies, not 14.
        assert!(similar_titles(
            "Quarterly report 2026",
            "Quarterly re-xxxxxxxxxx"
        ));
        assert!(!similar_titles(
            "Quarterly report 2026",
            "Quarterly r-xxxxxxxxxxx"
        ));
        // The order of the titles doesn't matter.
        assert!(similar_titles("Repo-99", "Report"));
        assert!(!similar_titles("Rep-999", "Report"));
    }

    /// R22: two-thirds of the shorter title is rounded down, as in Rooms'
    /// `SlotMatcher.similar`, which the spec names as the definition.
    #[test]
    fn r22_similar_prefix_threshold_rounds_two_thirds_down() {
        // Shorter title 4: 2 characters.
        assert!(similar_titles("Plan", "Plxx"));
        assert!(!similar_titles("Plan", "Pxxx"));
        // Shorter title 7: 4 characters.
        assert!(similar_titles("Q3 plan", "Q3 pitch"));
        assert!(!similar_titles("Q3 plan", "Q3 xitch"));
        // Shorter title 10: 6 characters.
        assert!(similar_titles("Project AB", "Projec-99999"));
        assert!(!similar_titles("Project AB", "Proje-999999"));
    }

    /// R22: a blank title never matches at step 2 or step 3. A record
    /// without a title still matches its window server id at step 1.
    #[test]
    fn r22_blank_titles_never_match_at_steps_2_and_3() {
        let (cx, _) = with_records(&[(
            "A",
            vec![
                empty_record("App", "", None),
                empty_record("App", "   ", None),
                empty_record("App", "    ", None),
            ],
        )]);
        for title in ["", "   ", "    "] {
            let w = window(1, 1, "App", title);
            assert_eq!(match_window(&w, &cx, MatchPass::Arrival), vec![], "{title:?}");
        }
        let (cx, ids) = with_records(&[("A", vec![empty_record("App", "", Some(1001))])]);
        assert_eq!(
            steps(&match_window(&window(1, 1, "App", ""), &cx, MatchPass::Arrival)),
            vec![(Slot::Context(ids[0]), MatchStep::WindowServerId)]
        );
    }

    /// R22: without a bundle id on both sides, the same app means the same
    /// app name.
    #[test]
    fn r22_same_app_falls_back_to_the_app_name() {
        let record = MemberRecord {
            bundle_id: None,
            app_name: Some("Tool".into()),
            title: "Main".into(),
            window_server_id: None,
            link: RecordLink::Empty,
        };
        let (cx, ids) = with_records(&[("A", vec![record])]);
        let exact = vec![(Slot::Context(ids[0]), MatchStep::ExactTitle)];
        let tool = WindowDesc {
            bundle_id: None,
            ..window(1, 1, "Tool", "Main")
        };
        assert_eq!(steps(&match_window(&tool, &cx, MatchPass::Arrival)), exact);
        let with_bundle_id = window(1, 2, "Tool", "Main");
        assert_eq!(
            steps(&match_window(&with_bundle_id, &cx, MatchPass::Arrival)),
            exact
        );
        let other = WindowDesc {
            bundle_id: None,
            ..window(2, 1, "Other", "Main")
        };
        assert_eq!(match_window(&other, &cx, MatchPass::Arrival), vec![]);
    }

    /// R22 step 4 fills only the switch target's records. Another context's
    /// record and a pinned record of the same app stay empty, and a switch to
    /// Everything or Unsorted runs no step 4, so an unsorted window stays
    /// unsorted and shows under Unsorted.
    #[test]
    fn r22_step_4_runs_only_for_the_switch_target() {
        let (mut cx, ids) = with_records(&[
            ("A", vec![empty_record("Chrome", "Docs", None)]),
            ("B", vec![empty_record("Chrome", "Calendar", None)]),
        ]);
        cx.pinned.push(empty_record("Chrome", "Music", None));
        let gmail = window(1, 1, "Chrome", "Gmail - Inbox");
        for target in [ContextKey::Everything, ContextKey::Unsorted] {
            assert_eq!(cx.rejoin(&gmail, MatchPass::Switch { target }), vec![]);
        }
        cx.switch_to(ContextKey::Unsorted).unwrap();
        let plan = plan_switch(&one_screen(cx.active(), vec![cx.switch_window(gmail.wid)]));
        assert_eq!(plan.park, vec![]);
        assert_eq!(
            cx.rejoin(&gmail, MatchPass::Switch { target: named(ids[1]) }),
            vec![RecordMatch {
                slot: Slot::Context(ids[1]),
                index: 0,
                step: MatchStep::SameApp,
            }]
        );
        assert_eq!(cx.contexts_of(gmail.wid), vec![ids[1]]);
        assert!(!cx.is_pinned(gmail.wid));
        assert_eq!(records(&cx, ids[0]), vec![("Docs", RecordLink::Empty)]);
        assert_eq!(cx.pinned()[0].link, RecordLink::Empty);
    }

    fn by_window<T>(windows: &[WindowDesc], results: Vec<T>) -> Vec<(WindowId, T)> {
        let mut pairs: Vec<(WindowId, T)> = windows.iter().map(|w| w.wid).zip(results).collect();
        pairs.sort_by_key(|(wid, _)| *wid);
        pairs
    }

    /// R22: during a switch, a window with a record's exact title gets the
    /// record, and another window of the app doesn't take it at step 4,
    /// whatever the order of the windows.
    #[test]
    fn r22_a_switch_matches_windows_the_same_in_any_order() {
        let (cx, ids) = with_records(&[("A", vec![empty_record("Chrome", "Docs", None)])]);
        let gmail = window(1, 1, "Chrome", "Gmail");
        let docs = window(1, 2, "Chrome", "Docs");
        for windows in [[gmail.clone(), docs.clone()], [docs.clone(), gmail.clone()]] {
            let mut cx = cx.clone();
            let matches = cx.rejoin_all(&windows, MatchPass::Switch { target: named(ids[0]) });
            assert_eq!(
                by_window(&windows, matches),
                vec![
                    (gmail.wid, vec![]),
                    (docs.wid, vec![exact_title(Slot::Context(ids[0]), 0)]),
                ]
            );
            assert!(cx.is_unsorted(gmail.wid));
        }
    }

    /// R20, R22: windows that appear together are matched together. A
    /// window with a similar title doesn't take the record of a window with
    /// the exact title, whatever their order, and joins the active context.
    #[test]
    fn r22_windows_that_appear_together_match_the_same_in_any_order() {
        let (mut cx, ids) = with_records(&[
            (
                "A",
                vec![empty_record("Zed", "contexts.rs — butterflyray", None)],
            ),
            ("B", vec![]),
        ]);
        cx.switch_to(named(ids[1])).unwrap();
        let other = window(1, 1, "Zed", "contexts.rs — sugarglider");
        let own = window(1, 2, "Zed", "contexts.rs — butterflyray");
        assert!(similar_titles(&other.title, &own.title));
        for windows in [[other.clone(), own.clone()], [own.clone(), other.clone()]] {
            let mut cx = cx.clone();
            let arrivals = cx.windows_appeared(&windows, cx.active());
            assert_eq!(
                by_window(&windows, arrivals),
                vec![
                    (other.wid, Arrival::Joined(ids[1])),
                    (
                        own.wid,
                        Arrival::Rejoined(vec![exact_title(Slot::Context(ids[0]), 0)])
                    ),
                ]
            );
        }
    }

    /// R21, R38: windows found at launch rejoin their contexts or stay
    /// unsorted. They never join the saved active context, as a new window
    /// would.
    #[test]
    fn r38_windows_found_at_launch_rejoin_or_stay_unsorted() {
        let mut before = Contexts::new();
        let a = before.create("A").unwrap();
        let b = before.create("B").unwrap();
        before.add_window(a, &window(1, 1, "Mail", "Inbox")).unwrap();
        before.switch_to(named(b)).unwrap();
        let mut cx = round_trip(&before);
        let inbox = window(2, 1, "Mail", "Inbox");
        let notes = window(3, 1, "Notes", "Notes");
        assert_eq!(
            cx.rejoin_all(&[inbox.clone(), notes.clone()], MatchPass::Arrival),
            vec![vec![exact_title(Slot::Context(a), 0)], vec![]]
        );
        assert_eq!(cx.active(), named(b));
        assert_eq!(cx.contexts_of(inbox.wid), vec![a]);
        assert!(cx.is_unsorted(notes.wid));
        assert_eq!(records(&cx, b), vec![]);
    }

    /// R22: once the window server ids are forgotten, a record without a
    /// window no longer matches at step 1. Live records keep their ids.
    #[test]
    fn r22_forgotten_window_server_ids_match_nothing() {
        let (mut cx, ids) =
            with_records(&[("A", vec![empty_record("App", "Old title", Some(1001))])]);
        let w = window(1, 1, "App", "New title");
        assert_eq!(match_window(&w, &cx, MatchPass::Arrival).len(), 1);
        let live = window(2, 1, "App", "Live");
        cx.add_window(ids[0], &live).unwrap();
        cx.pin(&live);
        cx.forget_window_server_ids();
        assert_eq!(match_window(&w, &cx, MatchPass::Arrival), vec![]);
        let ids_of = |records: &[MemberRecord]| {
            records.iter().map(|m| m.window_server_id).collect::<Vec<_>>()
        };
        assert_eq!(
            ids_of(&cx.get(ids[0]).unwrap().members),
            vec![None, live.window_server_id]
        );
        assert_eq!(ids_of(cx.pinned()), vec![live.window_server_id]);
    }

    /// R23, Q4: ⌘Q where every window closes before the app terminates. The
    /// records stay with their last titles and rejoin after a relaunch.
    #[test]
    fn r23_quit_with_windows_closed_before_termination() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let docs = window(1, 1, "Chrome", "New Tab");
        let mail = window(1, 2, "Chrome", "Mail");
        cx.add_window(a, &docs).unwrap();
        cx.add_window(b, &docs).unwrap();
        cx.add_window(b, &mail).unwrap();
        cx.title_changed(docs.wid, "Docs");
        cx.window_closed(docs.wid);
        cx.window_closed(mail.wid);
        assert_eq!(
            records(&cx, b),
            vec![
                ("Docs", RecordLink::Pending(docs.wid)),
                ("Mail", RecordLink::Pending(mail.wid)),
            ]
        );
        cx.app_terminated(1);
        assert_eq!(records(&cx, a), vec![("Docs", RecordLink::Empty)]);
        assert_eq!(
            records(&cx, b),
            vec![("Docs", RecordLink::Empty), ("Mail", RecordLink::Empty)]
        );
        let docs_again = window(2, 1, "Chrome", "Docs");
        let mail_again = window(2, 2, "Chrome", "Mail");
        assert_eq!(
            cx.window_appeared(&docs_again, ContextKey::Everything),
            Arrival::Rejoined(vec![
                exact_title(Slot::Context(a), 0),
                exact_title(Slot::Context(b), 0),
            ])
        );
        assert_eq!(
            cx.window_appeared(&mail_again, ContextKey::Everything),
            Arrival::Rejoined(vec![exact_title(Slot::Context(b), 1)])
        );
    }

    /// R23, Q4: ⌘Q where the app terminates before its windows report
    /// closed. The late closes change nothing.
    #[test]
    fn r23_quit_with_windows_closed_after_termination() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let docs = window(1, 1, "Chrome", "Docs");
        let mail = window(1, 2, "Chrome", "Mail");
        cx.add_window(a, &docs).unwrap();
        cx.add_window(a, &mail).unwrap();
        cx.app_terminated(1);
        cx.window_closed(docs.wid);
        cx.window_closed(mail.wid);
        assert_eq!(
            records(&cx, a),
            vec![("Docs", RecordLink::Empty), ("Mail", RecordLink::Empty)]
        );
        let docs_again = window(2, 1, "Chrome", "Docs");
        assert_eq!(
            cx.window_appeared(&docs_again, ContextKey::Everything),
            Arrival::Rejoined(vec![exact_title(Slot::Context(a), 0)])
        );
    }

    /// R23, Q4: ⌘Q where one window closes before the app terminates and
    /// the other after.
    #[test]
    fn r23_quit_with_one_window_closed_before_and_one_after_termination() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let docs = window(1, 1, "Chrome", "Docs");
        let mail = window(1, 2, "Chrome", "Mail");
        cx.add_window(a, &docs).unwrap();
        cx.add_window(a, &mail).unwrap();
        cx.window_closed(docs.wid);
        cx.app_terminated(1);
        cx.window_closed(mail.wid);
        assert_eq!(
            records(&cx, a),
            vec![("Docs", RecordLink::Empty), ("Mail", RecordLink::Empty)]
        );
    }

    /// R23, Q4: a window-server update that lists the app's remaining window
    /// between the first close and termination shows the app is still
    /// running, so the first window's records go. Q4 asks whether macOS
    /// sends such an update during ⌘Q.
    #[test]
    fn r23_quit_with_an_update_listing_a_remaining_window_before_termination() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let docs = window(1, 1, "Chrome", "Docs");
        let mail = window(1, 2, "Chrome", "Mail");
        cx.add_window(a, &docs).unwrap();
        cx.add_window(a, &mail).unwrap();
        cx.add_window(b, &docs).unwrap();
        cx.window_closed(docs.wid);
        cx.app_still_running(1);
        cx.app_terminated(1);
        cx.window_closed(mail.wid);
        assert_eq!(records(&cx, a), vec![("Mail", RecordLink::Empty)]);
        assert_eq!(records(&cx, b), vec![]);
        let docs_again = window(2, 1, "Chrome", "Docs");
        let mail_again = window(2, 2, "Chrome", "Mail");
        assert_eq!(
            cx.window_appeared(&docs_again, ContextKey::Everything),
            Arrival::Unsorted
        );
        assert_eq!(
            cx.window_appeared(&mail_again, ContextKey::Everything),
            Arrival::Rejoined(vec![exact_title(Slot::Context(a), 0)])
        );
    }

    /// R23: another app's termination or activity leaves pending records
    /// alone.
    #[test]
    fn r23_other_apps_leave_pending_records_alone() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let closed = window(1, 1, "App", "Closed");
        cx.add_window(a, &closed).unwrap();
        cx.window_closed(closed.wid);
        cx.app_terminated(2);
        cx.app_still_running(2);
        assert_eq!(
            records(&cx, a),
            vec![("Closed", RecordLink::Pending(closed.wid))]
        );
    }

    /// R23: records from an app's earlier run stay when the relaunched app
    /// shows it is running, so windows it opens later can still rejoin.
    #[test]
    fn r23_a_relaunched_app_keeps_the_records_of_windows_still_to_come() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        cx.add_window(a, &window(1, 1, "Chrome", "Docs")).unwrap();
        cx.add_window(a, &window(1, 2, "Chrome", "Mail")).unwrap();
        cx.app_terminated(1);
        let docs = window(2, 1, "Chrome", "Docs");
        cx.window_appeared(&docs, ContextKey::Everything);
        cx.app_still_running(2);
        assert_eq!(
            records(&cx, a),
            vec![
                ("Docs", RecordLink::Live(docs.wid)),
                ("Mail", RecordLink::Empty),
            ]
        );
        let mail = window(2, 2, "Chrome", "Mail");
        assert_eq!(
            cx.window_appeared(&mail, ContextKey::Everything),
            Arrival::Rejoined(vec![exact_title(Slot::Context(a), 1)])
        );
    }

    /// R23: a window closed for good leaves every context and the pinned
    /// list.
    #[test]
    fn r23_a_window_closed_for_good_leaves_every_context_and_the_pinned_list() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let shared = window(1, 1, "App", "Shared");
        let mail = window(1, 2, "App", "Mail");
        cx.add_window(a, &shared).unwrap();
        cx.add_window(a, &mail).unwrap();
        cx.add_window(b, &shared).unwrap();
        cx.pin(&shared);
        cx.window_closed(shared.wid);
        cx.app_still_running(1);
        assert_eq!(records(&cx, a), vec![("Mail", RecordLink::Live(mail.wid))]);
        assert_eq!(records(&cx, b), vec![]);
        assert_eq!(cx.pinned(), &[]);
    }

    /// `remove_record` removes a record from a context or the pinned list,
    /// whether or not its window is open.
    #[test]
    fn remove_record_removes_records_with_or_without_a_window() {
        let (mut cx, ids) = with_records(&[("A", vec![empty_record("App", "Gone", None)])]);
        let open = window(1, 1, "App", "Open");
        cx.add_window(ids[0], &open).unwrap();
        cx.pinned.push(empty_record("Music", "Music", None));
        let removed = cx.remove_record(Slot::Context(ids[0]), 0).unwrap();
        assert_eq!(removed, empty_record("App", "Gone", None));
        assert_eq!(records(&cx, ids[0]), vec![("Open", RecordLink::Live(open.wid))]);
        assert_eq!(
            cx.remove_record(Slot::Context(ids[0]), 1),
            Err(ContextError::NoSuchRecord)
        );
        assert_eq!(
            cx.remove_record(Slot::Context(ContextId(99)), 0),
            Err(ContextError::NoSuchContext)
        );
        assert!(cx.remove_record(Slot::Context(ids[0]), 0).is_ok());
        assert!(cx.is_unsorted(open.wid));
        assert!(cx.remove_record(Slot::Pinned, 0).is_ok());
        assert_eq!(cx.pinned(), &[]);
    }

    fn titled(prefix: &str, count: usize) -> Vec<String> {
        (0..count).map(|i| format!("{prefix} {i}")).collect()
    }

    /// When an app quits, a context keeps at most 50 empty records. The
    /// ones that were empty already go first, oldest first, then the app's
    /// own. Live records don't count.
    #[test]
    fn app_terminated_keeps_50_empty_records_dropping_the_oldest() {
        let old = titled("Old", 50);
        let (mut cx, ids) = with_records(&[(
            "A",
            old.iter().map(|title| empty_record("Gone", title, None)).collect(),
        )]);
        let docs = window(1, 1, "Chrome", "Docs");
        let mail = window(1, 2, "Chrome", "Mail");
        let notes = window(2, 1, "Notes", "Notes");
        for w in [&docs, &mail, &notes] {
            cx.add_window(ids[0], w).unwrap();
        }
        cx.window_closed(mail.wid);
        cx.app_terminated(1);
        let titles: Vec<&str> = records(&cx, ids[0]).into_iter().map(|(t, _)| t).collect();
        let mut expected: Vec<&str> = old[2..].iter().map(String::as_str).collect();
        expected.extend(["Docs", "Mail", "Notes"]);
        assert_eq!(titles, expected);

        // The app's own records go when they alone are over the limit.
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let own = titled("Own", 52);
        for (idx, title) in own.iter().enumerate() {
            let w = window(3, idx as u32, "App", title);
            cx.add_window(a, &w).unwrap();
            cx.pin(&w);
        }
        cx.app_terminated(3);
        let titles: Vec<&str> = records(&cx, a).into_iter().map(|(t, _)| t).collect();
        assert_eq!(titles, own[2..].iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(cx.pinned().len(), MAX_EMPTY_RECORDS);
        assert_eq!(cx.pinned()[0].title, "Own 2");
    }

    /// `contexts.json` keeps every record, because every record is empty
    /// after a restart, even in a context with more than 50 members. An app
    /// without records in the context doesn't trim it when it quits. An app
    /// with a record there does.
    #[test]
    fn contexts_json_keeps_every_record() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        for idx in 0..60 {
            cx.add_window(a, &window(1, idx, "App", &format!("W {idx}"))).unwrap();
        }
        let mut cx = round_trip(&cx);
        assert_eq!(cx.get(a).unwrap().members.len(), 60);
        let other = window(2, 1, "Other", "Other");
        cx.add_window(b, &other).unwrap();
        cx.app_terminated(2);
        assert_eq!(cx.get(a).unwrap().members.len(), 60);
        assert_eq!(records(&cx, b), vec![("Other", RecordLink::Empty)]);
        let late = window(3, 1, "Late", "Late");
        cx.add_window(a, &late).unwrap();
        cx.app_terminated(3);
        let titles: Vec<&str> = records(&cx, a).into_iter().map(|(t, _)| t).collect();
        assert_eq!(titles.len(), MAX_EMPTY_RECORDS);
        assert_eq!((titles[0], titles[49]), ("W 11", "Late"));
    }

    /// R24: focus on a member of the active context, pinned or not, never
    /// switches. An unsorted window switches to Unsorted unless Unsorted is
    /// already active.
    #[test]
    fn r24_focus_switches_only_for_windows_outside_the_active_context() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let shared = window(1, 1, "App", "Shared");
        let music = window(2, 1, "Music", "Music");
        let loose = window(3, 1, "Loose", "Loose");
        cx.add_window(a, &shared).unwrap();
        cx.add_window(b, &shared).unwrap();
        cx.pin(&music);
        cx.switch_to(named(b)).unwrap();
        cx.switch_to(named(a)).unwrap();
        let targets = |cx: &Contexts| [&shared, &music, &loose].map(|w| cx.focus_target(w.wid));
        assert_eq!(targets(&cx), [named(a), named(a), ContextKey::Unsorted]);
        cx.switch_to(ContextKey::Unsorted).unwrap();
        assert_eq!(
            targets(&cx),
            [named(a), ContextKey::Unsorted, ContextKey::Unsorted]
        );
    }

    /// R24: a new window's membership is decided before its focus counts, so
    /// a launched app never switches to Unsorted, and a window that rejoined
    /// another context switches there.
    #[test]
    fn r24_a_new_window_is_placed_before_its_focus_counts() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        cx.add_window(b, &window(1, 1, "Mail", "Inbox")).unwrap();
        cx.app_terminated(1);
        cx.switch_to(named(a)).unwrap();
        let launched = window(2, 1, "Notes", "Notes");
        assert_eq!(cx.window_appeared(&launched, cx.active()), Arrival::Joined(a));
        assert_eq!(cx.focus_target(launched.wid), named(a));
        let relaunched = window(3, 1, "Mail", "Inbox");
        assert_eq!(
            cx.window_appeared(&relaunched, cx.active()),
            Arrival::Rejoined(vec![exact_title(Slot::Context(b), 0)])
        );
        assert_eq!(cx.focus_target(relaunched.wid), named(b));
    }

    /// R27, R32, R33: showing Everything, which quitting and turning off
    /// also do, puts back every parked window on every screen, including
    /// windows on Spaces nobody sees. The focus never goes to one of those.
    #[test]
    fn r32_everything_puts_back_parked_windows_on_every_screen_and_space() {
        let input = SwitchInput {
            only: None,
            screens: vec![
                SwitchScreen {
                    active: ContextKey::Everything,
                    windows: vec![
                        SwitchWindow {
                            last_focus: Some(1),
                            ..member_of(wid(1, 1), &[A])
                        },
                        SwitchWindow {
                            parked: true,
                            invisible: true,
                            last_focus: Some(9),
                            ..member_of(wid(1, 2), &[B])
                        },
                    ],
                },
                SwitchScreen {
                    active: ContextKey::Everything,
                    windows: vec![
                        SwitchWindow {
                            parked: true,
                            invisible: true,
                            ..member_of(wid(2, 1), &[])
                        },
                        SwitchWindow {
                            parked: true,
                            invisible: true,
                            ..member_of(wid(2, 2), &[A])
                        },
                        SwitchWindow {
                            parked: true,
                            last_focus: Some(5),
                            ..member_of(wid(2, 3), &[B])
                        },
                    ],
                },
            ],
        };
        assert_eq!(
            plan_switch(&input),
            SwitchPlan {
                moves: vec![],
                park: vec![],
                unpark: vec![wid(1, 2), wid(2, 1), wid(2, 2), wid(2, 3)],
                focus: Some(wid(2, 3)),
            }
        );
    }

    /// R3, R24: focusing a pinned window never switches, whichever context
    /// is active, even when the window is also in another context.
    #[test]
    fn r24_focusing_a_pinned_window_never_switches() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let music = window(1, 1, "Music", "Music");
        cx.add_window(b, &music).unwrap();
        cx.pin(&music);
        for active in [
            named(a),
            named(b),
            ContextKey::Unsorted,
            ContextKey::Everything,
        ] {
            cx.switch_to(active).unwrap();
            assert_eq!(cx.focus_target(music.wid), active);
        }
        cx.unpin(music.wid);
        cx.switch_to(named(a)).unwrap();
        assert_eq!(cx.focus_target(music.wid), named(b));
    }

    /// R27: Everything puts back every parked window, including ones that
    /// aren't in the visible-window set and pinned ones.
    #[test]
    fn r27_everything_puts_back_parked_windows_in_any_state() {
        let windows = vec![
            SwitchWindow {
                parked: true,
                invisible: true,
                ..member_of(wid(1, 1), &[A])
            },
            SwitchWindow {
                parked: true,
                invisible: true,
                ..member_of(wid(1, 2), &[])
            },
            SwitchWindow {
                parked: true,
                pinned: true,
                ..member_of(wid(1, 3), &[B])
            },
            member_of(wid(1, 4), &[B]),
        ];
        let plan = plan_switch(&one_screen(ContextKey::Everything, windows));
        assert_eq!(plan.unpark, vec![wid(1, 1), wid(1, 2), wid(1, 3)]);
        assert_eq!(plan.park, vec![]);
    }

    /// R28: with no contexts, which is the model's state while the feature
    /// flag is off, Everything stays active, new windows get no records,
    /// focus never switches, and a switch moves no window.
    #[test]
    fn r28_without_contexts_nothing_changes() {
        let mut cx = Contexts::new();
        let windows = [window(1, 1, "App", "One"), window(2, 1, "Other", "Two")];
        for w in &windows {
            assert_eq!(cx.window_appeared(w, cx.active()), Arrival::Unsorted);
            cx.window_focused(w.wid);
            assert_eq!(cx.focus_target(w.wid), ContextKey::Everything);
        }
        let plan = plan_switch(&one_screen(
            cx.active(),
            windows.iter().map(|w| cx.switch_window(w.wid)).collect(),
        ));
        assert_eq!(plan.park, vec![]);
        assert_eq!(plan.unpark, vec![]);
        cx.title_changed(windows[0].wid, "Renamed");
        cx.window_closed(windows[0].wid);
        cx.app_terminated(1);
        cx.app_still_running(2);
        assert_eq!(cx.active(), ContextKey::Everything);
        assert_eq!(
            serde_json::to_value(&cx).unwrap(),
            serde_json::json!({
                "version": 1,
                "next_id": 1,
                "use_seq": 0,
                "contexts": [],
                "pinned": [],
                "active": { "global": "everything" }
            })
        );
    }

    /// R29: Unsorted's members are computed from membership. A pinned window
    /// shows under Unsorted but isn't counted as unsorted.
    #[test]
    fn r29_unsorted_membership_follows_the_named_contexts() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let w = window(1, 1, "App", "W");
        let state =
            |cx: &Contexts| (cx.is_unsorted(w.wid), cx.is_member(ContextKey::Unsorted, w.wid));
        assert_eq!(state(&cx), (true, true));
        cx.add_window(a, &w).unwrap();
        assert_eq!(state(&cx), (false, false));
        cx.pin(&w);
        assert_eq!(state(&cx), (false, true));
        cx.remove_window(a, w.wid).unwrap();
        assert_eq!(state(&cx), (false, true));
        cx.unpin(w.wid);
        assert_eq!(state(&cx), (true, true));
        cx.add_window(a, &w).unwrap();
        cx.delete(a).unwrap();
        assert_eq!(state(&cx), (true, true));
    }

    /// Switcher ranking from entries the caller builds, as the switcher
    /// bridge does from the published snapshot: the kind of match decides
    /// first, recent use second, and equal entries keep their order.
    #[test]
    fn rank_entries_ranks_by_match_then_recent_use_and_keeps_the_entry_order() {
        let mut cx = Contexts::new();
        let crew = cx.create("Crew").unwrap();
        let client = cx.create("Client work").unwrap();
        let comms = cx.create("Comms").unwrap();
        cx.switch_to(named(comms)).unwrap();
        let entries = |cx: &Contexts| -> Vec<(ContextKey, String, u64)> {
            cx.contexts()
                .iter()
                .map(|c| (named(c.id), c.name.clone(), c.last_used))
                .chain([(
                    ContextKey::Everything,
                    EVERYTHING_NAME.to_string(),
                    cx.last_used(ContextKey::Everything),
                )])
                .collect()
        };
        assert_eq!(
            rank_entries("c", entries(&cx)),
            vec![
                (named(comms), NameMatch::NamePrefix),
                (named(crew), NameMatch::NamePrefix),
                (named(client), NameMatch::NamePrefix),
            ]
        );
        assert_eq!(
            rank_entries("client", entries(&cx)),
            vec![(named(client), NameMatch::NamePrefix)]
        );
        assert_eq!(rank_entries("zzz", entries(&cx)), vec![]);
    }

    /// Switcher ranking: the kind of match decides first, recent use second.
    #[test]
    fn rank_orders_by_kind_of_match_before_recent_use() {
        let mut cx = Contexts::new();
        let ids = ["Crew", "Client work", "Big cwm", "CWS", "CW"].map(|n| cx.create(n).unwrap());
        // The weakest match is the most recently used.
        for id in ids.iter().rev() {
            cx.switch_to(named(*id)).unwrap();
        }
        assert_eq!(
            rank("cw", &cx, true),
            vec![
                (named(ids[4]), NameMatch::Exact),
                (named(ids[3]), NameMatch::NamePrefix),
                (named(ids[2]), NameMatch::WordPrefix),
                (named(ids[1]), NameMatch::Initials),
                (named(ids[0]), NameMatch::LettersInOrder),
            ]
        );
        let mut cx = Contexts::new();
        let ids = ["Clown town", "Client work", "Cl wonder"].map(|n| cx.create(n).unwrap());
        for id in ids.iter().rev() {
            cx.switch_to(named(*id)).unwrap();
        }
        assert_eq!(
            rank("cl wo", &cx, true),
            vec![
                (named(ids[2]), NameMatch::NamePrefix),
                (named(ids[1]), NameMatch::AllWordPrefixes),
                (named(ids[0]), NameMatch::LettersInOrder),
            ]
        );
    }

    /// Switcher ranking: ties go to the most recently used entry, Everything
    /// and Unsorted included. A blank query lists every entry.
    #[test]
    fn rank_ties_go_to_the_most_recently_used_entry_including_built_ins() {
        let mut cx = Contexts::new();
        let evening = cx.create("Evening").unwrap();
        let comms = cx.create("Comms").unwrap();
        let unsure = cx.create("Unsure").unwrap();
        cx.switch_to(named(evening)).unwrap();
        cx.switch_to(ContextKey::Everything).unwrap();
        cx.switch_to(named(comms)).unwrap();
        cx.switch_to(ContextKey::Unsorted).unwrap();
        for blank in ["", "   "] {
            assert_eq!(
                ranked_names(blank, &cx, true),
                vec!["Unsorted", "Comms", "Everything", "Evening", "Unsure"]
            );
        }
        assert_eq!(ranked_names("ev", &cx, true), vec!["Everything", "Evening"]);
        assert_eq!(ranked_names("uns", &cx, true), vec!["Unsorted", "Unsure"]);
        cx.switch_to(named(unsure)).unwrap();
        cx.switch_to(named(evening)).unwrap();
        assert_eq!(ranked_names("ev", &cx, true), vec!["Evening", "Everything"]);
        assert_eq!(ranked_names("uns", &cx, true), vec!["Unsure", "Unsorted"]);
    }

    /// Switcher ranking: an exact match ignores case and accents, for named
    /// contexts and for Everything and Unsorted.
    #[test]
    fn rank_exact_match_ignores_case_and_accents() {
        let mut cx = Contexts::new();
        let cafe = cx.create("Café Crème").unwrap();
        assert_eq!(
            rank("CAFE CREME", &cx, true),
            vec![(named(cafe), NameMatch::Exact)]
        );
        assert_eq!(
            rank("everything", &cx, true),
            vec![(ContextKey::Everything, NameMatch::Exact)]
        );
        assert_eq!(
            rank("UNSORTED", &cx, true),
            vec![(ContextKey::Unsorted, NameMatch::Exact)]
        );
    }

    /// Switcher ranking: the words of a query match as prefixes in any
    /// order, as in Rooms' `Matcher.rank`.
    #[test]
    fn rank_all_words_match_as_prefixes_in_any_order() {
        assert_eq!(
            match_of("wo cl", "Client work"),
            Some(NameMatch::AllWordPrefixes)
        );
    }

    /// Switcher ranking: a query without letters or digits matches nothing.
    #[test]
    fn rank_a_query_without_letters_or_digits_matches_nothing() {
        let mut cx = Contexts::new();
        cx.create("Comms").unwrap();
        cx.create("Client work").unwrap();
        for query in ["-", "?", "–", "()"] {
            assert_eq!(rank(query, &cx, true), vec![], "{query:?}");
        }
    }

    /// Switcher ranking: as in Rooms' `Matcher.rank`, letters in order need
    /// a query of at least 2 characters, so one character matches only the
    /// start of a word.
    #[test]
    fn rank_one_character_matches_only_word_starts() {
        let mut cx = Contexts::new();
        cx.create("Comms").unwrap();
        cx.create("Client work").unwrap();
        assert_eq!(ranked_names("w", &cx, false), vec!["Client work"]);
        assert_eq!(ranked_names("o", &cx, false), Vec::<String>::new());
    }

    /// Switcher ranking compares names without accents, including accents
    /// outside Latin-1 and Latin Extended-A.
    #[test]
    fn rank_ignores_accents_outside_latin_1_and_extended_a() {
        let mut cx = Contexts::new();
        let hanoi = cx.create("Hà Nội").unwrap();
        let lu = cx.create("Lǚ Xíng").unwrap();
        assert_eq!(
            rank("ha noi", &cx, false),
            vec![(named(hanoi), NameMatch::Exact)]
        );
        assert_eq!(rank("lu xing", &cx, false), vec![(named(lu), NameMatch::Exact)]);
    }

    /// `fold` folds every letter of Latin-1 Supplement and Latin Extended-A
    /// to lowercase ASCII. The expected values follow the letters' Unicode
    /// names.
    #[test]
    fn fold_folds_every_latin_1_and_extended_a_letter() {
        let table = [
            ("ÀÁÂÃÄÅàáâãäåĀāĂăĄą", "a"),
            ("Ææ", "ae"),
            ("ÇçĆćĈĉĊċČč", "c"),
            ("ÐðĎďĐđ", "d"),
            ("ÈÉÊËèéêëĒēĔĕĖėĘęĚě", "e"),
            ("ĜĝĞğĠġĢģ", "g"),
            ("ĤĥĦħ", "h"),
            ("ÌÍÎÏìíîïĨĩĪīĬĭĮįİı", "i"),
            ("Ĳĳ", "ij"),
            ("Ĵĵ", "j"),
            ("Ķķĸ", "k"),
            ("ĹĺĻļĽľĿŀŁł", "l"),
            ("ÑñŃńŅņŇňŉŊŋ", "n"),
            ("ÒÓÔÕÖØòóôõöøŌōŎŏŐő", "o"),
            ("Œœ", "oe"),
            ("ŔŕŖŗŘř", "r"),
            ("ŚśŜŝŞşŠšſ", "s"),
            ("ß", "ss"),
            ("ŢţŤťŦŧ", "t"),
            ("Þþ", "th"),
            ("ÙÚÛÜùúûüŨũŪūŬŭŮůŰűŲų", "u"),
            ("Ŵŵ", "w"),
            ("ÝýÿŶŷŸ", "y"),
            ("ŹźŻżŽž", "z"),
        ];
        for (letters, folded) in table {
            for letter in letters.chars() {
                assert_eq!(
                    fold(&letter.to_string()),
                    folded,
                    "U+{:04X} {letter}",
                    letter as u32
                );
            }
        }
        let mut covered: Vec<char> =
            table.iter().flat_map(|(letters, _)| letters.chars()).collect();
        covered.sort();
        let letters: Vec<char> =
            ('\u{C0}'..='\u{17F}').filter(|c| !matches!(c, '×' | '÷')).collect();
        assert_eq!(covered, letters);
    }

    /// `fold` drops every combining diacritical mark.
    #[test]
    fn fold_drops_every_combining_diacritical_mark() {
        for mark in '\u{300}'..='\u{36F}' {
            assert_eq!(fold(&format!("A{mark}b")), "ab", "U+{:04X}", mark as u32);
        }
    }

    /// `fold` folds every letter with a mark, and every digraph, of Latin
    /// Extended-B and Latin Extended Additional to lowercase ASCII. The
    /// other letters of those blocks are only lowercased.
    #[test]
    fn fold_folds_latin_extended_b_and_extended_additional() {
        let kept = "ƄƅƆƍƎƏƐƔƖƛƜƦƧƨƩƪƱƷƸƹƺƻƼƽƾƿǀǁǂǃǝǮǯǷȜȝɁɂɅẟ";
        for c in ('\u{0180}'..='\u{024F}').chain('\u{1E00}'..='\u{1EFF}') {
            let folded = fold(&c.to_string());
            if kept.contains(c) {
                assert_eq!(folded, c.to_lowercase().to_string(), "U+{:04X}", c as u32);
            } else {
                assert!(
                    !folded.is_empty() && folded.chars().all(|f| f.is_ascii_lowercase()),
                    "U+{:04X} {c} folds to {folded:?}",
                    c as u32
                );
            }
        }
        assert_eq!(fold("Hà Nội, Phở Đường"), "ha noi, pho duong");
        assert_eq!(fold("Lǚ Xíng Nǚ"), "lu xing nu");
        assert_eq!(fold("Ștefan Țară"), "stefan tara");
        assert_eq!(fold("Ǆ ǅ ǆ Ǉ ǌ ẞ Ỻ ƕ"), "dz dz dz lj nj ss ll hv");
    }

    /// R4: names are unique ignoring accents as well as case, as the
    /// switcher compares them. The reserved names are too.
    #[test]
    fn r4_names_are_unique_ignoring_accents() {
        let mut cx = Contexts::new();
        let cafe = cx.create("Café").unwrap();
        for variant in ["Cafe", "CAFE", "cafe\u{301}"] {
            assert_eq!(cx.create(variant), Err(ContextError::NameTaken("Café".into())));
        }
        let other = cx.create("Other").unwrap();
        assert_eq!(
            cx.rename(other, "cafe"),
            Err(ContextError::NameTaken("Café".into()))
        );
        assert_eq!(cx.by_name("CAFE").map(|c| c.id), Some(cafe));
        assert_eq!(
            cx.create("Évérything"),
            Err(ContextError::ReservedName("Évérything".into()))
        );
        cx.rename(cafe, "Cafe").unwrap();
        assert_eq!(context_names(&cx), vec!["Cafe", "Other"]);
    }

    /// `contexts.json` with fields this version doesn't know loads, and a
    /// missing `active` loads as Everything.
    #[test]
    fn contexts_json_ignores_unknown_fields_and_a_missing_active() {
        let doc = serde_json::json!({
            "version": 1,
            "next_id": 3,
            "use_seq": 7,
            "written_by": "a later Sugarglider",
            "contexts": [
                { "id": 2, "name": "Comms", "number": 4, "last_used": 7, "color": "blue",
                  "members": [
                      { "bundle_id": "net.whatsapp.WhatsApp", "app_name": "WhatsApp",
                        "title": "WhatsApp", "window_server_id": 81234, "frame": [0, 25] }
                  ] }
            ],
            "pinned": [
                { "bundle_id": "com.apple.Music", "title": "Music", "space": 3 }
            ]
        });
        let cx: Contexts = serde_json::from_value(doc).unwrap();
        assert_eq!(cx.active(), ContextKey::Everything);
        assert_eq!(
            cx.contexts().to_vec(),
            vec![Context {
                id: ContextId(2),
                name: "Comms".into(),
                number: Some(4),
                members: vec![MemberRecord {
                    bundle_id: Some("net.whatsapp.WhatsApp".into()),
                    app_name: Some("WhatsApp".into()),
                    title: "WhatsApp".into(),
                    window_server_id: Some(WindowServerId(81234)),
                    link: RecordLink::Empty,
                }],
                last_used: 7,
            }]
        );
        assert_eq!(
            cx.pinned().to_vec(),
            vec![MemberRecord {
                bundle_id: Some("com.apple.Music".into()),
                app_name: None,
                title: "Music".into(),
                window_server_id: None,
                link: RecordLink::Empty,
            }]
        );
        assert_eq!(
            serde_json::to_value(&cx).unwrap()["active"],
            serde_json::json!({ "global": "everything" })
        );
    }

    /// `contexts.json` keeps membership, numbers, the active context, and
    /// the use order. The previous context and focus order start fresh.
    #[test]
    fn contexts_json_keeps_membership_and_use_order_across_a_restart() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        let w = window(1, 1, "App", "W");
        cx.add_window(a, &w).unwrap();
        cx.set_number(a, Some(7)).unwrap();
        cx.switch_to(named(a)).unwrap();
        cx.switch_to(named(b)).unwrap();
        cx.window_focused(w.wid);
        let restored = round_trip(&cx);
        assert_eq!(restored.active(), named(b));
        assert_eq!(restored.previous(), None);
        assert_eq!([named(a), named(b)].map(|key| restored.last_used(key)), [1, 2]);
        assert_eq!(restored.last_focus(w.wid), None);
        assert_eq!(
            restored
                .contexts()
                .iter()
                .map(|c| (c.id, c.name.as_str(), c.number))
                .collect::<Vec<_>>(),
            vec![(a, "A", Some(7)), (b, "B", Some(2))]
        );
        assert_eq!(records(&restored, a), vec![("W", RecordLink::Empty)]);
    }

    /// Context ids are never reused, after a delete or a restart.
    #[test]
    fn context_ids_are_never_reused() {
        let mut cx = Contexts::new();
        let a = cx.create("A").unwrap();
        let b = cx.create("B").unwrap();
        cx.delete(b).unwrap();
        let c = cx.create("C").unwrap();
        let mut restored = round_trip(&cx);
        restored.delete(c).unwrap();
        let d = restored.create("D").unwrap();
        assert_eq!([a, b, c, d].map(ContextId::get), [1, 2, 3, 4]);
    }

    /// Text that isn't a JSON object is an error, so the caller can move the
    /// file aside.
    #[test]
    fn contexts_json_that_is_not_an_object_is_an_error() {
        for text in ["", "null", "[]", "{", "not json"] {
            assert!(serde_json::from_str::<Contexts>(text).is_err(), "{text:?}");
        }
    }

    /// Extreme ids and use numbers in `contexts.json` don't panic. The file
    /// is refused, or it loads with fresh unique ids and a working use order.
    #[test]
    fn contexts_json_with_extreme_counters_never_overflows() {
        let doc = serde_json::json!({
            "version": 1,
            "next_id": 1,
            "use_seq": 0,
            "contexts": [
                { "id": u32::MAX, "name": "Last", "last_used": u64::MAX }
            ]
        });
        let Ok(mut cx) = serde_json::from_value::<Contexts>(doc) else {
            return;
        };
        let fresh = cx.create("Fresh").unwrap();
        assert_ne!(fresh, ContextId(u32::MAX));
        cx.switch_to(named(fresh)).unwrap();
        assert!(cx.last_used(named(fresh)) > cx.last_used(named(ContextId(u32::MAX))));
    }

    /// R5: `contexts.json` loads with numbers that aren't a context number,
    /// and drops them.
    #[test]
    fn contexts_json_drops_numbers_of_any_other_value() {
        let numbers = [
            serde_json::json!(300),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("3"),
            serde_json::json!(u64::MAX),
            serde_json::json!(9),
        ];
        let doc = serde_json::json!({
            "version": 1,
            "contexts": numbers.iter().enumerate().map(|(i, number)| {
                serde_json::json!({ "id": i + 1, "name": format!("C{i}"), "number": number })
            }).collect::<Vec<_>>()
        });
        let cx: Contexts = serde_json::from_value(doc).unwrap();
        assert_eq!(
            cx.contexts().iter().map(|c| c.number).collect::<Vec<_>>(),
            vec![None, None, None, None, None, Some(9)]
        );
    }

    /// R4: in a loaded `contexts.json`, an empty, reserved, or taken name
    /// gets a number, and the first context with a name keeps it.
    #[test]
    fn contexts_json_numbers_empty_reserved_and_taken_names() {
        let names = [
            " Work ",
            "Comms",
            "comms",
            "Everything",
            " unsorted ",
            " ",
            "",
            "Café",
            "Cafe",
        ];
        let doc = serde_json::json!({
            "version": 1,
            "contexts": names.iter().enumerate().map(|(i, name)| {
                serde_json::json!({ "id": i + 1, "name": name })
            }).collect::<Vec<_>>()
        });
        let cx: Contexts = serde_json::from_value(doc).unwrap();
        assert_eq!(
            context_names(&cx),
            vec![
                "Work",
                "Comms",
                "comms 2",
                "Everything 2",
                "unsorted 2",
                "Context 1",
                "Context 2",
                "Café",
                "Cafe 2",
            ]
        );
    }

    /// `contexts.json` without `next_id` and `use_seq` takes them from its
    /// contexts.
    #[test]
    fn contexts_json_without_counters_takes_them_from_its_contexts() {
        let doc = serde_json::json!({
            "version": 1,
            "contexts": [{ "id": 4, "name": "A", "last_used": 6 }]
        });
        let mut cx: Contexts = serde_json::from_value(doc).unwrap();
        let b = cx.create("B").unwrap();
        assert_eq!(b, ContextId(5));
        cx.switch_to(named(b)).unwrap();
        assert_eq!(cx.last_used(named(b)), 7);
    }

    /// Context ids stay unique after they run out at `u32::MAX`. A new
    /// context then takes the lowest free id.
    #[test]
    fn context_ids_stay_unique_after_they_run_out() {
        let doc = serde_json::json!({
            "version": 1,
            "next_id": u32::MAX,
            "use_seq": 0,
            "contexts": [
                { "id": 1, "name": "A" },
                { "id": u32::MAX, "name": "B" },
                { "id": u32::MAX, "name": "C" }
            ]
        });
        let mut cx: Contexts = serde_json::from_value(doc).unwrap();
        assert_eq!(cx.create("D").unwrap(), ContextId(3));
        let ids = |cx: &Contexts| cx.contexts().iter().map(|c| c.id.get()).collect::<Vec<_>>();
        assert_eq!(ids(&cx), vec![1, u32::MAX, 2, 3]);
        cx.delete(ContextId(2)).unwrap();
        assert_eq!(cx.create("E").unwrap(), ContextId(2));
        assert_eq!(ids(&cx), vec![1, u32::MAX, 3, 2]);
    }

    /// R19: when the use numbers run out, they are numbered again from 1 in
    /// the same order, and the next switch is still the most recent use.
    #[test]
    fn r19_use_order_survives_running_out_of_use_numbers() {
        let doc = serde_json::json!({
            "version": 1,
            "next_id": 4,
            "use_seq": u64::MAX,
            "contexts": [
                { "id": 1, "name": "A", "last_used": u64::MAX - 1 },
                { "id": 2, "name": "B", "last_used": u64::MAX },
                { "id": 3, "name": "C", "last_used": 0 }
            ]
        });
        let mut cx: Contexts = serde_json::from_value(doc).unwrap();
        cx.switch_to(ContextKey::Unsorted).unwrap();
        cx.switch_to(named(ContextId(3))).unwrap();
        assert_eq!([1, 2, 3].map(|id| cx.last_used(named(ContextId(id)))), [1, 2, 4]);
        assert_eq!(cx.last_used(ContextKey::Unsorted), 3);
        assert_eq!(
            ranked_names("", &cx, true),
            vec!["C", "Unsorted", "B", "A", "Everything"]
        );
    }

    /// Comms (1, used last), Client work (2), and Relax (no number), and
    /// the id of a deleted context.
    fn three_to_resolve() -> (Contexts, [ContextId; 3], ContextId) {
        let mut cx = Contexts::new();
        let comms = cx.create("Comms").unwrap();
        let client = cx.create("Client work").unwrap();
        let relax = cx.create("Relax").unwrap();
        let gone = cx.create("Gone").unwrap();
        cx.delete(gone).unwrap();
        cx.set_number(relax, None).unwrap();
        cx.switch_to(named(comms)).unwrap();
        (cx, [comms, client, relax], gone)
    }

    /// Commands and dispatch, R4, R29. What each form of query names, with
    /// Unsorted listed. A number or an id names its context. A name names
    /// a context by its exact name, ignoring case and accents, or else by
    /// the switcher's ranking among the named contexts. The built-in
    /// entries match only their exact names.
    #[test]
    fn resolve_takes_numbers_ids_exact_names_and_ranked_names() {
        let (cx, [comms, client, relax], gone) = three_to_resolve();
        let no_match = |name: &str| Err(ContextError::NoMatch(name.to_string()));
        let table: Vec<(Query, Result<ContextKey, ContextError>)> = vec![
            (Query::Number(1), Ok(named(comms))),
            (Query::Number(2), Ok(named(client))),
            (Query::Number(3), Err(ContextError::NoContextNumbered(3))),
            (Query::Number(0), Err(ContextError::NoContextNumbered(0))),
            (Query::Number(9), Err(ContextError::NoContextNumbered(9))),
            (Query::Id(relax), Ok(named(relax))),
            (Query::Id(gone), Err(ContextError::NoSuchContext)),
            (Query::Name("Comms"), Ok(named(comms))),
            (Query::Name("  COMMS  "), Ok(named(comms))),
            (Query::Name("cómms"), Ok(named(comms))),
            (Query::Name("Client work"), Ok(named(client))),
            (Query::Name("cli"), Ok(named(client))),
            (Query::Name("cw"), Ok(named(client))),
            (Query::Name("work"), Ok(named(client))),
            (Query::Name("rlx"), Ok(named(relax))),
            (Query::Name("Everything"), Ok(ContextKey::Everything)),
            (Query::Name("ÉVERYTHING"), Ok(ContextKey::Everything)),
            (Query::Name(" unsorted "), Ok(ContextKey::Unsorted)),
            (Query::Name("uns"), no_match("uns")),
            (Query::Name("every"), no_match("every")),
            (Query::Name("Sugarglider"), no_match("Sugarglider")),
            (Query::Name(" 7 "), no_match("7")),
            (Query::Name("--"), no_match("--")),
            (Query::Name(""), Err(ContextError::NoQuery)),
            (Query::Name(" \t "), Err(ContextError::NoQuery)),
        ];
        for (query, expected) in table {
            assert_eq!(expected, resolve(query, &cx, true), "{query:?}");
        }
        assert_eq!(
            "No context matches \"uns\"",
            resolve(Query::Name("uns"), &cx, true).unwrap_err().to_string()
        );
        assert_eq!(
            "No context has the number 3",
            resolve(Query::Number(3), &cx, true).unwrap_err().to_string()
        );
    }

    /// Commands and dispatch, R19. A partial name takes the named context
    /// it matches, even when a built-in entry that it also starts was used
    /// more recently. A partial name that only a built-in entry matches
    /// names nothing.
    #[test]
    fn resolve_takes_a_named_context_over_a_more_recently_used_built_in() {
        let mut cx = Contexts::new();
        let unicorn = cx.create("Unicorn").unwrap();
        let email = cx.create("Email").unwrap();
        for key in [
            named(unicorn),
            named(email),
            ContextKey::Unsorted,
            ContextKey::Everything,
        ] {
            cx.switch_to(key).unwrap();
        }

        assert_eq!(Ok(named(unicorn)), resolve(Query::Name("un"), &cx, true));
        assert_eq!(Ok(named(email)), resolve(Query::Name("e"), &cx, true));
        for partial in ["ev", "every", "unsort", "uns"] {
            assert_eq!(
                Err(ContextError::NoMatch(partial.to_string())),
                resolve(Query::Name(partial), &cx, true),
                "{partial}"
            );
        }
    }

    /// R28, R29. Unsorted's name names it only while it is listed, and
    /// never before the first context exists. Everything's name always
    /// names Everything.
    #[test]
    fn resolve_names_unsorted_only_while_it_is_listed_and_a_context_exists() {
        let none = Contexts::new();
        let (cx, ..) = three_to_resolve();
        let unsorted = Query::Name("Unsorted");
        let no_match = Err(ContextError::NoMatch("Unsorted".to_string()));

        assert_eq!(no_match, resolve(unsorted, &none, true));
        assert_eq!(no_match, resolve(unsorted, &none, false));
        assert_eq!(no_match, resolve(unsorted, &cx, false));
        assert_eq!(Ok(ContextKey::Unsorted), resolve(unsorted, &cx, true));
        for (contexts, listed) in [(&none, false), (&none, true), (&cx, false)] {
            assert_eq!(
                Ok(ContextKey::Everything),
                resolve(Query::Name("everything"), contexts, listed)
            );
        }
    }

    /// R29. While Unsorted isn't listed, its exact name names nothing, even
    /// when it starts a context's name. A shorter name still takes that
    /// context.
    #[test]
    fn resolve_never_takes_unsorteds_name_for_a_context_that_it_starts() {
        let mut cx = Contexts::new();
        let work = cx.create("Unsorted work").unwrap();

        for name in ["Unsorted", " UNSORTED "] {
            assert_eq!(
                Err(ContextError::NoMatch(name.trim().to_string())),
                resolve(Query::Name(name), &cx, false),
                "{name}"
            );
            assert_eq!(Ok(ContextKey::Unsorted), resolve(Query::Name(name), &cx, true));
        }
        for listed in [false, true] {
            assert_eq!(Ok(named(work)), resolve(Query::Name("unsort"), &cx, listed));
            assert_eq!(Ok(named(work)), resolve(Query::Name("Unsorted w"), &cx, listed));
        }
    }
}

# Switcher JSON contract

This is the contract between the context switcher panel (Swift, `SugargliderUI`) and Rust. The Swift half implements it in `SugargliderUI/Sources/SugargliderUI/ContextSwitcherContract.swift`. The doc comment on `ContextSwitcherJSON` there summarizes it and repeats the examples. The examples below are also the test fixtures in `SugargliderUI/Tests/SugargliderUITests/ContractFixtures.swift`, so the Swift tests decode and re-encode exactly these literals. A test checks that the three copies of the examples are the same.

Design source: `docs/specs/contexts.md`, sections "Switcher" and "Swift bridge". That section lists a Swift-to-Rust function `sugarglider_get_contexts()`. This contract replaces it with `sugarglider_rank_contexts(query)`. The show payload already carries what `get_contexts` would return, and ranking stays in one place, `model::contexts::rank`, which the command line also uses (I3).

## General rules

- All keys are snake_case.
- A named context is always tagged as `{"id": N}`, never a bare number, so it can't be read as a context number (1–9).
- The strings `"everything"` and `"unsorted"` name the two built-in entries. This key type is not the key-binding `ContextRef`, where a bare string is a name. Rust must not parse these fields as `ContextRef`. The switcher never sends a name, except as the new name in `create` and `rename`.
- A window is Rust's `WindowId` in its serde shape, `{"pid": 812, "idx": 9123}` (`pid: pid_t`, `idx: NonZeroU32`). Rust can deserialize it with the existing derive. The switcher never reads the fields and sends the value back unchanged.
- Rust serializes every key whose type doesn't say "or null", empty arrays included. It puts no `skip_serializing_if` on those fields. A show payload without one of them doesn't decode, and the panel doesn't open.
- Swift ignores keys it doesn't know, so Rust can add keys without breaking an older panel.

## Functions

Rust to Swift, exported by SugargliderUI with `@_cdecl`:

| Symbol | Signature | Does |
|---|---|---|
| `sugarglider_show_context_switcher` | `fn(json: *const c_char)` | Shows a fresh panel (empty query, list view) with the show payload. When the panel is open, it closes the panel instead, so the switcher's hotkey toggles it. The reactor calls it from its own thread every time it handles `open_context_switcher`. Swift copies the string before it returns and shows or closes the panel on the main queue. Swift logs a NULL `json` and ignores it. It logs a payload that doesn't decode with `NSLog` and opens nothing. |
| `sugarglider_hide_context_switcher` | `fn()` | Hides the panel if it is open. Safe to call at any time, from any thread, and more than once. |

Rust doesn't track whether the panel is open. The panel closes itself on Esc, when it loses key status, and after a command succeeds. Rust doesn't learn of these closes. So Rust calls show for every `open_context_switcher`, and Swift decides whether that opens or closes the panel. Rust calls show for nothing else.

Swift to Rust, implemented by the Rust half:

| Symbol | Signature | Does |
|---|---|---|
| `sugarglider_rank_contexts` | `extern "C" fn(query: *const c_char) -> *mut c_char` | Returns `model::contexts::rank(query, contexts, unsorted_has_windows)` as the rank result below, computed from the published snapshot (I1). Returns NULL when contexts are unavailable, for example because the feature is off. |
| `sugarglider_run_context_command` | `extern "C" fn(json: *const c_char) -> *mut c_char` | Takes one command below. Returns NULL when Rust accepted the command and sent it to the reactor (`WmEvent::Command` through the sender in `CONFIG_UPDATE_SENDER`). Otherwise returns an error message for the user. |

- Each input string is NUL-terminated UTF-8 and is valid only during the call.
- Swift frees every returned string with the existing `sugarglider_free_string`.
- Swift looks up both Swift-to-Rust functions with `dlsym(RTLD_DEFAULT, "<name>")` when the panel needs them, not with `@_silgen_name`. A binary without them still loads, and the panel shows an inline error instead of crashing. They must therefore stay exported from the executable, like the config functions.
- Build requirements for the Rust half (spec, "Swift bridge"): add `-Wl,-u,_sugarglider_rank_contexts` and `-Wl,-u,_sugarglider_run_context_command` to the symbol list in `build.rs`, add a `black_box` reference to each in `swift_bridge::init`, and declare `sugarglider_show_context_switcher(json: *const c_char)` and `sugarglider_hide_context_switcher()` in the `extern "C"` block of `src/ui/swift_bridge.rs`.

Neither function may block. The panel calls `rank` on every keystroke and `run` on every command, synchronously on the main thread, and `WmController` runs on the main thread too. So:

- `rank` reads only the published snapshot. It never waits on the reactor or on `WmController`.
- `run` checks the command against the same snapshot, sends it through `CONFIG_UPDATE_SENDER` without waiting, and returns at once. It never waits for the reactor's reply. The reactor logs a failure that it finds later. `run` can't return it.

Both run outside the reactor, so the published snapshot must hold what they need. The shape that `sugarglider context list --json` prints (spec, "Command line") has less. The snapshot holds at least:

- every named context's id, name, and most recent use (`last_used`), for `rank` and for the id and name checks of `run`;
- the most recent use of Unsorted and of Everything, for `rank`;
- `unsorted_has_windows`: whether the Unsorted count below is more than 0, for `rank`;
- every context's member records, with their app as `members[].app` derives it, their title, and whether each has an open window, for the `remove_records` check of `run`.

`sugarglider_run_context_command` checks against the published snapshot before it sends the command, and returns an error for:

- invalid JSON, or a command it doesn't know;
- the feature is off;
- a context id that no longer exists;
- a `create` or `rename` name that is empty, reserved, or taken (R4). The panel checks a `create` name against the reserved names and the payload first, but the payload can be out of date. For `rename`, the panel checks only that the name isn't empty;
- a `set_number` number outside 1 to 9;
- an `edit` whose `remove_records` names a record that no longer matches (see `edit`). Then `run` sends nothing.

The reactor logs failures that come later, for example a failed journal write (R30).

### Commands in the reactor

Every command about a window carries the window, and Rust acts on that window, never on the window focused when the command arrives. The panel takes key focus while it is open, so the focused window can differ from the target window by then. The key-binding commands `add_window_to_context`, `move_window_to_context`, and `toggle_window_pinned` take no window and act on the reactor's main window. The switcher's commands must not reuse that path unchanged. For example, the reactor-side command can be `ContextCommand::AddWindow { window: Option<WindowId>, context: ContextRef }`, where `None` means the focused window for a key binding. The FFI always passes `Some`, and the reactor never falls back to its main window when it has `Some`.

- `{"id": N}` becomes `ContextRef::Id(N)`, never `ContextRef::Number(N)`.
- `create` with a window list and `edit` have no key-binding form.
- `Command` is `#[serde(untagged)]` and deserializes from TOML, so any new variant can also be written in the config. Its name must not collide with a `LayoutCommand` name, and it needs arms in `describe_command` and the other exhaustive matches (spec, "Commands and dispatch").

### When Rust hides the panel

Rust calls `sugarglider_hide_context_switcher`:

- after a context command succeeds, whatever sent it: a key binding, the command line, or the panel. The panel has closed itself already after its own command, and the call does nothing then. After a switch from elsewhere, the panel's active dot and payload are out of date.
- when the feature turns off, for example through a config reload (R33), or Sugarglider turns off;
- when an exit is pending (R32), because the reactor then ignores context commands.

## Show payload

```json
{
  "display_id": 1,
  "target_window": { "pid": 812, "idx": 9123 },
  "contexts": [
    {
      "id": 1, "name": "Sugarglider", "number": 1, "hotkey": "⌃⌥1",
      "active": true, "apps": ["Ghostty", "Zed", "Google Chrome"], "windows": 3,
      "members": [
        { "record": 0, "app": "Ghostty", "title": "~/src/sugarglider",
          "window": { "pid": 640, "idx": 8812 } },
        { "record": 1, "app": "Zed", "title": "reactor.rs",
          "window": { "pid": 701, "idx": 8920 } },
        { "record": 2, "app": "Google Chrome", "title": "Docs",
          "window": { "pid": 812, "idx": 9123 } },
        { "record": 3, "app": "Mail", "title": "Inbox", "window": null }
      ]
    },
    {
      "id": 4, "name": "Client work", "number": null, "hotkey": null,
      "active": false, "apps": [], "windows": 0, "members": []
    }
  ],
  "unsorted": { "windows": 2, "active": false },
  "everything": { "active": false, "hotkey": "⌃⌥0" },
  "windows": [
    { "id": { "pid": 640, "idx": 8812 }, "title": "~/src/sugarglider",
      "app": "Ghostty", "tab_count": 1, "pinned": false },
    { "id": { "pid": 812, "idx": 9123 }, "title": "Docs",
      "app": "Google Chrome", "tab_count": 3, "pinned": false },
    { "id": { "pid": 903, "idx": 9201 }, "title": "WhatsApp",
      "app": "WhatsApp", "tab_count": 1, "pinned": true },
    { "id": { "pid": 977, "idx": 9310 }, "title": "Downloads",
      "app": "Finder", "tab_count": 1, "pinned": false },
    { "id": { "pid": 988, "idx": 9402 }, "title": "general",
      "app": "Slack", "tab_count": 1, "pinned": false }
  ]
}
```

In this example the Zed window (record 1) is open but not on screen (another Space, minimized, or parked), and the Mail window (record 3) is gone. The Chrome window is the main tab of a group of 3 tabs. The WhatsApp window is pinned, so it doesn't count as unsorted; Finder and Slack are the 2 unsorted windows.

| Key | Type | Meaning |
|---|---|---|
| `display_id` | u32 or null | The `CGDirectDisplayID` of the focused screen. The panel centers on it. Null or an unknown id means the main screen. |
| `target_window` | window or null | The window that had focus when the reactor handled `open_context_switcher` (spec: "target window"). Every command about a window carries this id, never the window focused when the command is sent. It is null when no window had focus, or when the focused window is untracked, parked (for example Finder's window after R12 step 6), or Sugarglider's own. Otherwise `windows` holds it. Null disables ⌘↩, ⇧⌘↩, and ⌘P: the panel dims their hints and says why. |
| `contexts` | array | Every named context. |
| `contexts[].id` | u32 | The context id. |
| `contexts[].name` | string | The name. |
| `contexts[].number` | 1–9 or null | The number (R5). |
| `contexts[].hotkey` | string or null | A label for a key binding that switches to the context, or null when none does. Rust computes it from any `switch_context` binding whose `ContextRef` resolves to the context, by number, name, or id. When several do, Rust picks one, for example the first in config order. It formats the label like the Preferences hotkey list (`format_hotkey`, for example `"⌃⌥1"`). The panel shows the string as it is, or the bare number when it is null. |
| `contexts[].active` | bool | Whether this is the active context. |
| `contexts[].apps` | string array | The distinct app names of the context's open member windows, in member order. The panel shows the first three. |
| `contexts[].windows` | integer | How many member windows are open. |
| `contexts[].members` | array | Every member record, in the model's order (`Context.members`). The edit view lists the ones that aren't in `windows`. |
| `members[].record` | integer | The record's index in `Context.members`, as `Contexts::remove_record` takes it. |
| `members[].app` | string | The app name: `app_name`, or the bundle id when the name is unknown, or `"Unknown app"` when both are unknown. `run` compares `remove_records` items with the same derived string. |
| `members[].title` | string | The record's title. |
| `members[].window` | window or null | The record's open window, or null when the window is gone (the record is empty or pending, R23). The key may be missing, which means null. |
| `unsorted.windows` | integer | The number of unsorted windows: tracked windows on the visible Spaces that are in no named context. A pinned window is in every context (R3), so it doesn't count. The snapshot's `unsorted_has_windows` uses the same count, so the rank result and the payload agree on whether Unsorted is listed (R29). |
| `unsorted.active` | bool | Whether Unsorted is active. |
| `everything.active` | bool | Whether Everything is active. |
| `everything.hotkey` | string or null | The binding that runs `show_everything`, or null. |
| `windows` | array | The windows on screen that the create and edit views list: the tracked windows that show now on the visible Spaces (the focused screen in `per_screen` scope), including the target window. Leave out parked windows, Sugarglider's own windows, and untracked windows. A native tab group is one entry, its main tab (R36). |
| `windows[].id` | window | The window. |
| `windows[].title`, `windows[].app` | string | Its title, and its app name or `"Unknown app"`. |
| `windows[].tab_count` | integer | How many tabs the window's native tab group has, or 1 for a window without tabs. The create and edit views show "(n tabs)" when it is more than 1. |
| `windows[].pinned` | bool | Whether it is pinned (R3). The create and edit views show a pinned window checked and fixed. |

Exactly one of the `active` flags is true. Keys whose type says "or null" may also be missing.

Tabs that share a frame share membership (R36). Every window id in the payload (`target_window`, `windows[].id`, and `members[].window`) names a tab group by its main tab, so the panel shows one row per group. Every command that carries a window (`add_window`, `move_window`, `toggle_pinned`, `create.windows`, `edit.add`, and `edit.remove`) applies to the window's whole group. Rust resolves a tab to its group, as R36 requires.

Which contexts hold a window is only in `contexts[].members`: a window is a member of a context when one of the context's records has it as `window`. The edit view checks a window on screen by that rule.

## Rank result

The entries of `rank`, best first. `key` is `{"id": N}`, `"unsorted"`, or `"everything"`. `match` is the `NameMatch` variant in snake_case (`#[serde(rename_all = "snake_case")]`): `exact`, `name_prefix`, `word_prefix`, `initials`, `all_word_prefixes`, `letters_in_order`, or `empty_query`.

For the query `"cli"`:

```json
[
  { "key": { "id": 4 }, "match": "name_prefix" }
]
```

For the empty query, every entry, most recently used first. Unsorted is listed only when it has windows (R29):

```json
[
  { "key": { "id": 1 }, "match": "empty_query" },
  { "key": { "id": 4 }, "match": "empty_query" },
  { "key": "unsorted", "match": "empty_query" },
  { "key": "everything", "match": "empty_query" }
]
```

How the panel uses it:

- It calls rank for every query, the empty one included, and shows the entries in this order. It skips ids that the show payload doesn't have.
- It shows the "New context “<text>”" row, last, when no entry has `match` equal to `exact` and the trimmed query can name a new context: it isn't empty, it isn't "Everything" or "Unsorted", and it isn't the name of a context in the payload. The panel compares names lowercased and without the accents of Latin letters, as `model::contexts::fold` does. Where its comparison differs from `fold`, it folds less, so it never refuses a name that Rust accepts. So "unsorted" offers no "New context" row, even when the rank result leaves Unsorted out.
- The first-time state ("Type a name to create your first context") shows when the payload has no named contexts and Everything is active. Then the list shows no entries, only the "New context" row once the user types.
- When `sugarglider_rank_contexts` is missing or returns NULL, the panel shows an inline error and lists the payload's entries unfiltered, in payload order (named contexts, Unsorted when it has windows, Everything).

## Commands

Each command is an object with exactly one key, the command's name.

### `switch`

```json
{ "switch": { "id": 4 } }
```

```json
{ "switch": "unsorted" }
```

```json
{ "switch": "everything" }
```

Switches to the entry (R12). `"everything"` shows Everything (R27). Sent by ↩ on a context row.

### `add_window`

```json
{ "add_window": { "window": { "pid": 812, "idx": 9123 }, "context": { "id": 4 } } }
```

Adds `window`, the target window, to the context. The rules are those of `add_window_to_context` (R37), but Rust acts on the window that the command carries, never on the focused window (see "Commands in the reactor"). Sent by ⌘↩ on a named context.

### `move_window`

```json
{ "move_window": { "window": { "pid": 812, "idx": 9123 }, "context": { "id": 4 } } }
```

Moves `window`, the target window, out of the active context and into this one. The rules are those of `move_window_to_context` (R37), for the window that the command carries, never the focused one. Sent by ⇧⌘↩ on a named context.

### `toggle_pinned`

```json
{ "toggle_pinned": { "window": { "pid": 812, "idx": 9123 } } }
```

Pins or unpins `window`, the target window. The rules are those of `toggle_window_pinned` (R3), for the window that the command carries, never the focused one. Sent by ⌘P.

### `create`

```json
{ "create": { "name": "Sugarglider",
              "windows": [{ "pid": 640, "idx": 8812 }, { "pid": 812, "idx": 9123 }] } }
```

Creates a context with this name (R4) and the lowest free number (R5), whose members are exactly these windows, and switches to it, as `sugarglider context create` does. `windows` can be empty. Sent from the create view, which ↩ on the "New context" row opens. ⌘N first opens a naming view that holds the trimmed query, where the user can change the name. The panel checks the name as it does for the "New context" row before it lists the windows, and says why when the name can't be used. When Rust rejects `create`, the panel goes back to the naming view with the name and Rust's message, and keeps the windows the user checked.

`windows` never holds a pinned window. A pinned window is a member of every context already (R3), so the create view shows it checked and fixed and leaves it out. Rust ignores a pinned window in `windows` and gives it no record.

### `edit`

```json
{ "edit": { "context": { "id": 1 },
            "add": [{ "pid": 977, "idx": 9310 }],
            "remove": [{ "pid": 701, "idx": 8920 }],
            "remove_records": [{ "record": 3, "app": "Mail", "title": "Inbox" }] } }
```

Changes a context's members. Sent from the edit view (⌘E) with only what changed. In order, Rust:

1. Removes each record in `remove_records` (`Contexts::remove_record`, R23), from the highest `record` down. It removes a record only when the record at that index still has no open window and still has this app and title.

   R23 can change the list while the panel is open: a pending record goes when its app shows it is still running, and a quitting app trims the list to `MAX_EMPTY_RECORDS`. So `run` checks each item against the snapshot first. If any item doesn't match, `run` sends nothing and returns an error, for example `A closed window changed while the switcher was open. Open the switcher again.` If the list changes after that check and before the reactor applies the edit, the reactor skips the item that no longer matches and logs it.
2. Removes the windows in `remove`. This takes effect at once, as `remove_window_from_context` does (R37). A window here is open; it may be off screen.
3. Adds the windows in `add`. This takes effect at the next switch, as `add_window_to_context` does (R37).

The panel puts an unchecked member with an open window in `remove`, and one without a window in `remove_records`. `add` and `remove` never hold a pinned window: the edit view shows it checked and fixed. Rust ignores a pinned window in them, so its records stay as they are.

### `rename`

```json
{ "rename": { "context": { "id": 4 }, "name": "Client work 2026" } }
```

Renames the context (R4). Sent by ⌘R, then ↩. The panel sends the trimmed name and never an empty one.

### `set_number`

```json
{ "set_number": { "context": { "id": 4 }, "number": 2 } }
```

Gives the context a number from 1 to 9, and takes it from the context that had it (R5). Sent by ⌘1–⌘9.

### `delete`

```json
{ "delete": { "id": 4 } }
```

Deletes the context (R6). Sent by ⌘⌫, then ↩ on the inline confirm row.

## Error return

`sugarglider_run_context_command` returns NULL on success. On failure it returns a message for the user, for example:

```text
A context named "Comms" already exists
```

The panel shows it inline and stays open. After a command succeeds, the panel closes.

## Panel behavior that Rust relies on

- Built-in entries: the panel never sends `add_window`, `move_window`, `edit`, `rename`, `set_number`, or `delete` for Unsorted or Everything (R29). It shows an inline message instead.
- The panel is a non-activating panel, so opening it doesn't change the frontmost app. It closes when it loses key status, after a successful command, on Esc from the list, and on a second show.

## Keys

The panel follows the spec's keys table, with these details:

- ↑ and ↓ stop at the first and last row. They don't wrap around.
- With an empty query and the active context on the first row, the highlight starts on the second row, so ↩ goes back to the context used before it.
- Esc closes the panel from the list. In the naming, rename, create, and edit views and in the delete confirmation, Esc goes back to the list and keeps the query.
- The delete confirmation takes only ↩ and Esc. The other keys of the table do nothing there, and typing into the search field cancels the confirmation.
- While an input method composes text in a field, every key goes to the input method.
- ⌘X, ⌘C, ⌘V, ⌘A, ⌘Z, and ⇧⌘Z edit the text fields, because Sugarglider has no Edit menu to send them.

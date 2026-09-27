# Contexts

Status: M2 through M10 are integrated in this working branch behind `settings.experimental.contexts.enable`, which defaults to false. PR #25 remains draft pending physical QA. Written 2026-09-25, revised 2026-09-27.

Code references name symbols first; line numbers are hints and will drift.

Implemented in this integration: the model, switching and membership, parking and the journal, `contexts.json`, IPC, the full command line and Raycast script, menu bar, switcher, per-screen scope, Preferences scope picker, and the user guide.

The M1 physical spike has partial one-display TextEdit and Calculator evidence for Q1. Q1 for other apps and two displays, Q2 to Q4, Q5's target, and manual QA still need a person.

## Start here

This section is for whoever picks up the work next.

- This working branch integrates M2 through M10. PR #25 remains draft pending the M1 physical spike, Q5's target, and manual QA.
- The design decisions in [Settled decisions](#settled-decisions) came from the product owner. Build on them; don't reopen them.
- Q1 has partial one-display TextEdit and Calculator evidence. The remaining [physical questions](#what-we-dont-know-yet) and [manual QA](#testing) can still require changes.
- Agent sessions must not start the live window manager (`cargo run`, `sugarglider launch`). See `agents.md`. A person runs the spike and the manual QA checklist. Agents run `cargo test`, `cargo +nightly fmt --check`, and `devtool`.
- Rules are numbered (R1, R2, …) so commits and tests can cite them. Numbers never change. A new rule takes the next free number, and so do new design items (L, H, I) and questions (Q).

## Summary

A **context** is a named set of windows with its own tiling layout. Switching to a context shows its windows in that layout and hides every other window. Nothing is closed. One window can belong to several contexts.

You switch with a hotkey, the menu bar, a search panel called the **switcher**, or the `sugarglider context` command line. Raycast and SuperCmd can call the command line.

The idea comes from [Rooms](https://github.com/saragordic/rooms), an MIT-licensed Mac app. We take its switching model and its safety rules. We drop its layout templates, because Sugarglider already tiles.

## Problem

Sugarglider keeps one tiling layout per macOS Space. People who switch between several jobs keep all those windows open. On one Space, all of them share one layout. Tiles get small, or windows pile up in stacks.

macOS Spaces don't solve this. Each window lives on exactly one Space. A window manager can't move windows between Spaces unless the user partly disables System Integrity Protection, which is how yabai does it. So contexts work inside the Spaces that are visible, and they hide windows instead of moving them to other Spaces.

## Goals and non-goals

Goals:

- Switch the whole visible window set with one keystroke, or by typing a few letters of a context name.
- Give each context its own tiling layout.
- Let one window belong to several contexts.
- Never lose a window. Every window Sugarglider parks comes back after a quit, a crash, or a restart.
- Let launchers list and switch contexts.
- Change nothing for people who never create a context.

Non-goals for the first version:

- Browser tabs. A context holds windows. Put a project's tabs in their own browser window.
- Moving windows between macOS Spaces.
- Opening apps or windows that aren't running.
- Native full-screen windows and Stage Manager.
- Rooms' layout templates (Focus, Columns, Grid).

## Settled decisions

The product owner made these choices on 2026-09-25:

- The feature is called **context**. The search panel is called the **switcher**.
- A window can belong to several contexts. Example: WhatsApp is in both "Comms" and "Relax".
- Scope is a setting: `global` (a switch changes every screen) or `per_screen` (a switch changes only the focused screen).
- When the user focuses a window from another context, Sugarglider switches to that context.
- If two screens both want the same shared window, the window goes to the screen that switched most recently.
- Windows that are in no context appear in an "Unsorted" entry instead of being hidden everywhere.

And on 2026-09-26:

- Sugarglider hides windows only by parking them. It never hides apps.
- Global scope ships first. `per_screen` scope follows in its own milestone ([M9](#m9-per-screen-scope)).
- A window's records survive when its app quits, even if macOS reports the window closed before the app quit (R23).
- In `per_screen` scope, a window that a switch moves to another screen keeps its saved tile position in the layout of the screen it left. That screen closes the visible gap while the window is away, and the window returns to the saved position (R9).
- The partial Q1 result sets H1's current bound. A person still needs to test other apps and two displays, then complete Q2 to Q4 and manual QA.

## Terms

- **Context**: a named set of windows. It has its own layout on each Space, and on each screen size, like today's layouts.
- **Member**: a window in a context. Sugarglider records it by app bundle id, window title, and window server id.
- **Member record**: the stored description of a member. A record stays when the window is gone (for example, when the app quits), so the window can rejoin later.
- **Active context**: the context a screen shows. In `global` scope, all screens share one active context.
- **Everything**: a built-in view that shows every window in the Space's normal layout, the layout Sugarglider uses today. You can't add windows to it.
- **Unsorted**: a built-in context. Its members are the windows that belong to no named context.
- **Park a window**: move a window to a bottom corner so that no more than a 1 by 32 point strip stays on its display. Sugarglider parks every window that must not show.
- **Journal**: the file `~/.glide/parked.json`. Sugarglider writes each window there before it parks it.
- **Pinned window**: a window that is a member of every context.

## User stories

1. **Save a context.** You have the windows for a job open. You press ⌃⌥Space and type "Sugarglider". You choose "New context". The switcher lists the windows on screen, all checked. You uncheck the ones that don't belong and press Enter. The new context's layout starts as your current layout without the unchecked windows.
2. **Switch by name.** You press ⌃⌥Space, type "cli", and press Enter. The "Client work" windows appear in their layout. All other windows hide.
3. **Switch by number.** You press ⌃⌥2.
4. **Switch from a launcher.** In Raycast or SuperCmd you run "Switch Context" and type "cli".
5. **Keep working.** You open a new window. It joins the context you're in.
6. **Share a window.** WhatsApp is in "Comms" and "Relax". It has its own place in each layout.
7. **Fix a mistake.** A window is in the wrong context. You focus it and open the switcher. ⌘↩ adds it to the highlighted context. ⇧⌘↩ moves it there.
8. **Follow focus.** You ⌘-Tab to Chrome. Its windows are only in "Relax". Sugarglider switches to "Relax".
9. **Two monitors.** With `per_screen` scope, the left screen shows "Comms" and the right screen shows "Build".
10. **Get everything back.** ⌃⌥0 or "Show Everything" in the menu shows all windows. Quitting Sugarglider shows all windows. After a crash, the next launch shows them.
11. **An app restarts.** WhatsApp quits and opens again. Its window rejoins "Comms" and "Relax", because the app and the title match.

## Behavior rules

### Membership

- **R1.** A context holds windows, not apps. A window can be in any number of contexts.
- **R2.** A member has its own place in each context's layout. Each context keeps a layout per Space and per screen size, the same way Sugarglider keeps layouts today.
- **R3.** A pinned window is a member of every context, including contexts created later. It also shows under Unsorted, but it doesn't count as an unsorted window. `toggle_window_pinned` pins and unpins the focused window.
- **R4.** Context names are unique, ignoring case and accents. "Everything" and "Unsorted" are reserved.
- **R5.** A context can have a number from 1 to 9. Numbers are unique. A new context gets the lowest free number, or none when 1 to 9 are taken. Giving a context a number that another context holds takes the number away from that context.
- **R6.** Deleting a context never touches its windows. Windows that were only in that context become unsorted. If the deleted context was active, Unsorted becomes active, so those windows stay visible. This counts as a use of Unsorted (R19). The context's layouts are deleted with it (L2).
- **R36.** Native window tabs are separate windows that share one frame. Tabs that share a frame share membership. The group's main tab decides which contexts the whole group is in. Two windows are tabs only while both are on screen and neither is parked; a window that shares the last frame of one that is minimized, on another Space, or closed with ⌘W is not a tab of it.
- **R37.** `add_window_to_context` takes effect at the next switch command. The window stays visible now, even when adding it takes it out of an active Unsorted. Until that switch command, including one that applies the same context again (R16), H2's predicate treats the window as a member of the active context, so it keeps its tile. This is its grace. A macOS Space change re-applies the context (L2) but is not a switch command, so it doesn't end the grace. `move_window_to_context` and `remove_window_from_context` take effect at once. If the window no longer must show (R13), Sugarglider parks it (R30 first) and focuses the active context's most recently focused member.

### Scope and active context

R8, R9, R11, and R26 apply to `per_screen` scope, implemented in [M9](#m9-per-screen-scope).

- **R7.** In `global` scope, one active context covers all screens. A switch changes every screen. Windows stay on the screen they're on.
- **R8.** In `per_screen` scope, each screen has its own active context. A switch changes only the focused screen. The members that are on other visible screens move to the focused screen.
- **R9.** In `per_screen` scope, a shared window can only be on one screen. It goes to the screen that switched most recently. The other screen's layout closes the gap. The window returns when that screen switches again. The screen the window leaves keeps its saved tile position for that return, so the window comes back to its old place.
- **R10.** A screen's active context applies to whichever Space the screen shows. When the user changes Space, Sugarglider applies the active context to the newly visible Space. It does this in the reactor event that reports the change, before any other event reaches that Space's layout (L2).
- **R11.** Changing scope from `per_screen` to `global` makes the focused screen's context the global one. Changing from `global` to `per_screen` gives every screen the current global context. If the config changes to `global` while Sugarglider is stopped, startup waits until it knows the focused screen, then chooses that screen's saved context and clears the saved per-screen map before the next save. Before startup completes, a context command returns an error if the focused screen is still unknown. If startup completes before the first usable screen event, that event applies the chosen context again after reconciliation. When no focused window is known at startup completion, the first screen supplies the context.

### Switching

- **R12.** A switch to context C runs these steps in this order:
  1. Write the journal entries for every window the switch will park (R30). If a write fails, the switch stops here and the old context stays active.
  2. Make C's layout the active layout on each affected Space.
  3. Reconcile C's members (L8) and lay them out. Members that were parked return to their place in C's layout. Floating members return to their journal frame (H3).
  4. Park the windows that must not show.
  5. Focus a window. A switch that R24 started focuses the window the user focused. Any other switch focuses C's most recently focused member.
  6. If no window can take focus in step 5, activate Finder through the window server, the way the raise path makes an app frontmost, so keystrokes don't go to a parked window. A refused activation is reported, so the wait for Finder ends either way.

  Steps 5 and 6 also run when Sugarglider applies the active context outside a switch: at `StartupComplete`, and when a config reload turns contexts on. If the apply parked the window that had the focus, its context's most recently focused member takes the focus, or Sugarglider activates Finder when there is none (`apply_again_focusing_parked_main`).
- **R13.** A window "must show" when it is a member of the active context of the screen it is on, or will move to (R8). Under Everything, every window on that screen's visible Space must show.
- **R14.** Sugarglider never parks:
  - its own windows;
  - windows it doesn't track (`classify_window` returns `Untracked`);
  - windows that aren't in the reactor's visible-window set (`Reactor.visible_windows`). This covers minimized windows, which stay minimized, windows of apps that the user hid with ⌘H, and windows that are only on Spaces nobody can see (R10 handles them when their Space becomes visible).
- **R15.** Parking is the only way Sugarglider hides a window. It never hides an app or minimizes a window. Parked windows keep their place in every layout they belong to (L5).
- **R16.** Switching to the context that is already active applies it again. This parks windows that drifted in.
- **R17.** A switch has no animation. Windows appear in their final place.
- **R18.** `previous_context` switches back to the context used before the current one (per screen in `per_screen` scope). The previous context is never the active context and never a deleted one. A switch to the active context leaves it alone. When deleting the active context makes Unsorted active (R6), the context used before the deleted one stays the previous context, unless it is Unsorted.
- **R19.** Each switch updates a most-recently-used order. Deleting the active context counts as a use of Unsorted (R6). The switcher and R24 use this order.

### New and closed windows

- **R20.** A new window that matches no member record (R22, steps 1 to 3) joins the active context of the screen where it appears. If that screen shows Everything or Unsorted, the window is unsorted. Sugarglider never parks a new window that matched no record. A window is new only the first time Sugarglider sees it (R38). Only windows the layout tracks get membership: Sugarglider's own windows and windows `classify_window` calls `Untracked`, such as a panel that isn't on the normal layer, never join a context, by R20 or R21 (R14, R38).
- **R21.** A window that matches a member record (R22) rejoins the contexts that hold that record. It does not also join the active context. If none of its contexts is active, Sugarglider parks it like any other window that must not show. If the window takes focus, R24 applies.
- **R22.** Sugarglider matches windows to empty member records in this order:
  1. same app and same window server id;
  2. same app and exactly the same title, which isn't blank;
  3. same app and a similar title, and the window is in no context;
  4. any window of the same app that is in no context and is on screen, only during a switch and only for the records of the target context.

  Every step needs the same app. A window and a record belong to the same app when their bundle ids are equal. When either bundle id is unknown, their app names must be equal. Window server ids are valid within one login session, so after a reboot a saved id can name an unrelated window, even one of another app. M5a stores the boot id in `contexts.json`. At launch, when the boot id has changed, the reactor calls `Contexts::forget_window_server_ids` before any matching.

  Titles are similar when, after lowercasing and removing accents, both have at least 4 characters and one contains the other, or they share a prefix of at least min(12, two-thirds of the shorter title, rounded down). This is Rooms' `SlotMatcher.similar`. Step 3 never takes a blank title.

  Steps 3 and 4 take only windows that are in no context, aren't pinned, and matched no record at an earlier step, so they never take a window that belongs to another context. Step 4 never fills a pinned record, and a switch to Everything or Unsorted runs no step 4 (`MatchPass::Switch { target }`). Step 4 doesn't run when a window arrives (`MatchPass::Arrival`), because it would claim, and then park, a window the user just opened.

  Windows that appear together are matched together, as in Rooms' `SlotMatcher`. Each step runs for every window before the next step starts, so a weak match never takes a record that another window matches at an earlier step. A record takes at most one window, and a window takes at most one record from each context and from the pinned list (`match_windows`, `Contexts::rejoin_all`).

  A member record's title follows its live window. The app thread sends a title-change event from its `kAXTitleChangedNotification` branch (`src/actor/app.rs`, `handle_notification`), and the reactor updates `WindowState.title` and, only while the flag is on, the window's member records (`Contexts::title_changed`). With the flag off, a title change updates the window and leaves the records as they are. When the app quits, the record keeps the last title, so a relaunched Chrome or editor window can match it at step 2 or 3.
- **R23.** When a window closes, its records become pending. Matching skips pending records. If the app then terminates, the records stop being pending and stay, so the windows can rejoin when the app runs again (R21). If the app shows that it is still running, the records are deleted. An app shows this when the reactor first sees one of its windows, or when the user activates it. A later window-server update that lists one of its windows doesn't count, because an app that is quitting can list its remaining windows between a window's close and the app's termination (Q4). When Sugarglider restarts, all records stay.

  A single-window app that stays running after the user closes its window with ⌘W sends none of these signals. Its record stays pending until the app quits, and then it stays. This is accepted behavior.

  A context, and the pinned list, keeps at most 50 empty records (`MAX_EMPTY_RECORDS`). When a quitting app takes a list over that limit, the oldest empty records go. The records that were empty already go first, then the app's own, each in list order, which is the order the records were added. Lists that hold no record of the app stay as they are. Loading `contexts.json` applies no limit, because every record is empty after a restart. The user removes a record whose window is gone in the edit view or from the command line (`Contexts::remove_record`).
- **R38.** The reactor decides a window's membership once, the first time it sees the window (`WindowCreated`, `WindowsDiscovered`, or `ApplicationLaunched`). A window whose layer the window server hasn't listed yet waits; the list decides it when it arrives, and the app's own list decides a window the app reports there. This keeps a panel, whose layer is reported after its creation, out of the contexts (R20). The reactor keeps the set of windows it has seen, so R20 and R21 apply only to windows seen for the first time. A window that comes back from being minimized, from a hidden app, or from another Space is not new. Windows that the reactor discovers before `StartupComplete` were open before Sugarglider started. They rejoin their contexts through R21 (`Contexts::rejoin_all` with `MatchPass::Arrival`) or stay unsorted, and R20 never applies to them.

  If the initial AX window query fails after notification registration, the app thread stays registered with an empty first list and retries on the visibility poll. A nonempty AX list marks an omitted known window hidden only while the current WindowServer snapshot lists it; AX can omit windows on another Space. When AX lists that window again, the reactor restores its visible status from the current WindowServer snapshot.

  Before a user switch to a context other than Everything, the reactor rejects a normal-layer WindowServer row on the affected screen if AX has not identified it or has omitted a known nonmember. The old context and journal stay unchanged. If this mismatch occurs while an active context is reapplied, Sugarglider shows Everything and restores parked windows. A closed window can remain briefly in WindowServer's list after Cmd+W, so that case can also cause a fallback. The product owner chose this fail-safe behavior on 2026-09-27.
- **R39.** A known window that becomes visible without taking focus is parked when it must not show (R13), with its journal entry written first (R30). For example, the user unminimizes it or unhides its app, it moves in from another Space, or its app moves it back from its parking spot. If it takes focus, R24 applies instead. A repark uses fresh AX readback and the H1 bound. After five movements that need a repark, or one refused repark, Sugarglider shows Everything and restores parked windows. It does not leave an exposed nonmember under the context.

### Focus from outside

- **R24.** When the user focuses a window that is not a member of its screen's active context, Sugarglider switches to the most recently used context that contains the window. Ways to focus include ⌘-Tab, the Dock, a notification click, and launching an app. If the window is in no context, Sugarglider switches to Unsorted. If the screen shows Everything, nothing happens. These windows never cause a switch:
  - windows Sugarglider doesn't track, such as a Raycast or 1Password panel;
  - Sugarglider's own windows, including the switcher.

  Sugarglider decides a new window's membership (R20, R21, R38) before it applies this rule to the window, so launching an app never switches to Unsorted. If a focus event names a window the reactor hasn't seen yet, the rule waits and applies when R38 first sees the window. Sugarglider keeps each app's events in order, because an app thread sends its events and its window-server requests through one channel (`send_event` and `send_ws_request` in `src/actor/app.rs`). Spike question Q3 is only about the order in which macOS sends the Accessibility notifications.
- **R25.** Only focus the user started counts. Sugarglider's focus raise is not quiet. The raise manager sends the other raises of a sequence with `Quiet::Yes`, and the focus raise last, with `Quiet::No`, so that it produces `WindowFocused` (`src/actor/raise.rs`, `RaiseManager::start_new_sequence` and `process_active_sequence`). The reactor chooses each sequence's id and passes it in `RaiseRequest`. A switch therefore ignores activations and main-window changes until its wait ends. The focusing raise can end the wait only after the raise manager reports sending it (`RaiseFocusSent`), so a failure or timeout of an earlier batch doesn't end it. It ends on a completed raise of the switch's window, on a `RaiseRequestFailed` that names that window once the raise was sent, or on a `RaiseTimeout` after the raise was sent. A later switch waits for its own focusing raise or Finder activation and keeps earlier parking echoes for windows that remain parked. Each new focusing action starts a fresh 2-second focus deadline. When the switch issues no raise, the wait ends when the reactor has the `Requested(true)` frame echo of every window the switch parked. When step 6 of R12 runs, the wait also lasts until Finder's activation arrives or fails; the switch's own activation of Finder doesn't count as the user's focus. Focus that arrived while the wait was on is looked at again when it ends, unless it is the window the switch focused. The raise or Finder wait times out after 2 seconds and logs a warning (`GUARD_DEADLINE`, `guard_deadline_tick`). Parking confirmation has its own 2-second deadline. Focus stays guarded until AX reports the requested parked frame, parking fails or times out, or the window is put back before confirmation. Releasing a parked window cancels its parking echo without processing buffered focus during the new switch. Parking the focused window may make macOS send an activation or main-window change that looks like the user's (spike question Q2).
- **R26.** In `per_screen` scope, the switch happens on the screen where the window was last shown, or on the focused screen when that is unknown.
- **R40.** When the user activates an app whose main window is parked, and the app has a visible member of the active context, Sugarglider raises that member and doesn't switch. With several such members, it raises the most recently focused one. It switches (R24) only when the app has no member in the active context. For example, Chrome has window w1 in C and w2 in D. C is active, and Chrome's main window is still the parked w2. ⌘-Tab to Chrome raises w1. Only an activation of the app is this rule's trigger. A main-window change inside the app that is already frontmost, as ⌘\` or a Mission Control pick of a parked window makes, takes R24's path instead: Sugarglider switches to the context that holds the window.

### Everything and Unsorted

- **R27.** Showing Everything puts every parked window back at its journal frame, reconciles the Space's normal layout (L8), and lays it out.
- **R28.** Everything is active until the first context exists. With the feature flag off, Everything is always active. People who never create a context see no change, apart from three accepted deviations: the experimental scroll layouts (L9), `WindowAdded`'s check for a window that already has a node (L11), and, while contexts are in use, the window that leaves a Space also leaving its other screen sizes (L10).
- **R29.** Unsorted behaves like a context whose members are computed. Pinned windows show there too (R3). It has its own layouts. The switcher lists it only when it has unsorted windows, and the unsorted count leaves pinned windows out and is 0 until the first context exists. It can't be renamed, numbered, or deleted.

### Safety

- **R30.** Before Sugarglider parks a window, it writes the journal entry to disk. If the write fails, it doesn't park. In a switch, a failed write stops the whole switch, and the old context stays active (R12). If parking fails while reapplying a context after a Space or visibility change, Sugarglider shows Everything and restores the parked windows.
- **R31.** Sugarglider removes a journal entry only after it confirms the window is back, or when the window is gone. The window is back when the `Requested(true)` echo of the unpark write (`WindowFrameChanged` with `requested` set) reports a position within 16 points of the target's position (the tolerance Rooms uses); only the position counts, because some apps keep a size of their own. The window is gone on `WindowDestroyed`, or on `ApplicationThreadTerminated` for its app. The reactor clears a window's parked state when it sends the unpark write (H3). The journal entry goes later, when the echo confirms the frame.
- **R32.** On quit (the Quit menu item and `save_and_exit`, which both send `ReactorCommand::SaveAndExit`), Sugarglider puts every parked window back and reconciles the layouts, as R27 does. It doesn't change the saved active context, so `--restore` and the next launch return to that context. The exit waits for the windows:
  1. `SaveAndExit` starts the unpark and marks the journal entries it expects R31 to confirm: every entry whose window the reactor knows. An entry of an app that hasn't registered can only be put back at the next launch.
  2. While the exit is pending, Sugarglider parks nothing and ignores context commands.
  3. After R31 confirms the last marked entry, or when a fallback deadline passes, Sugarglider saves the layout and exits.
  4. If the deadline passes first, the journal stays on disk, and R34 puts the remaining windows back at the next launch.

  The deadline uses the reactor's existing 2-second visibility refresh timer (`visibility_timer` in `Reactor::run_reactor_loop`). The first tick that comes at least 2 seconds after `SaveAndExit` ends the wait. The deadline check takes the current `Instant` as an argument, so tests can call it directly. This timer use is a deliberate exception to the rule against new sources of nondeterminism in `CONTRIBUTING.md`. It keeps an app that never answers from blocking the quit.
- **R33.** When Sugarglider turns off (`toggle_global_enabled`, `sugarglider pause`, or turning off the feature flag), it shows Everything first. When the user turns off one space (`toggle_space_activated`), it shows Everything on that space first. `SpaceManager` handles the global switch and the space toggle (`src/actor/space_manager.rs`, `set_global_enabled` and `toggle_space`). The reactor sees only their result, a `None` Space in `SpaceChanged`, and by then it can't tell which Space to restore. So `SpaceManager` sends the reactor an explicit event, for example `Event::ShowEverythingOn(Vec<SpaceId>)`, before a disable or a toggle changes `active_spaces`. A config reload that turns off the flag reaches the reactor as `ConfigChanged`, and the reactor handles it there. The login window, a locked screen, `one_space`, `default_disable`, and Mission Control keep their current paths and don't show Everything.
- **R34.** On launch, Sugarglider restores the journal one app at a time. When an app's `ApplicationLaunched` or `WindowsDiscovered` event arrives, Sugarglider restores that app's journal entries. It applies no context to the app's windows until then. At `StartupComplete` it drops the entries whose process is gone or whose pid now belongs to another app, whether or not the reactor registered that app. If the journal can't be read, Sugarglider moves it to `parked.unreadable-<unix time>.json`, logs an error, and starts a new one.
- **R35.** If Sugarglider dies and nobody starts it again, the user must still be able to reach every window. H1 keeps a bottom-corner strip. In Q1, Mission Control selection and Window > Zoom did not recover one parked TextEdit window, but entering Full Screen made it usable. This macOS-native route is evidence for that window, not proof that every app and window can be recovered.

## User interface

### Switcher

`open_context_switcher` opens it. The suggested binding is ⌃⌥Space.

```
┌────────────────────────────────────────────────────┐
│ ⌕ Switch context…                                  │
├────────────────────────────────────────────────────┤
│ ● Sugarglider   Ghostty · Zed · Chrome    ⌃⌥1      │
│ ▸ Client work   Outlook · Word · Chrome   ⌃⌥2      │
│   Comms         Slack · Mail · Calendar   ⌃⌥3      │
│   Unsorted      2 windows                          │
│   Everything    show all windows          ⌃⌥0      │
├────────────────────────────────────────────────────┤
│ ↩ switch  ⌘↩ add window  ⇧⌘↩ move window  ⌘N new   │
└────────────────────────────────────────────────────┘
```

- ● marks the active context. ▸ marks the highlighted row.
- Each row shows the name, up to three app names, and the number shortcut.
- Typing filters the list. Ranking follows Rooms' `Matcher.rank`: exact name, then name prefix, then word prefix, then initials, then all words matched as prefixes, then letters in order. Ties go to the most recently used context. Names are compared lowercased and without accents.
- When no name matches exactly, a "New context “<text>”" row appears.
- The first time, the list is empty and says "Type a name to create your first context".

Keys:

| Key | Action |
|---|---|
| ↑ ↓ | Move the highlight |
| ↩ | Switch to the highlighted context |
| ⌘↩ | Add the target window to the highlighted context |
| ⇧⌘↩ | Move the target window to the highlighted context |
| ⌘N | New context from the windows on screen |
| ⌘E | Edit the highlighted context's windows |
| ⌘R | Rename the highlighted context |
| ⌘1 – ⌘9 | Give the highlighted context that number |
| ⌘P | Pin or unpin the target window |
| ⌘⌫ | Delete the highlighted context (R6) |
| Esc | Close |

The **target window** is the window that had focus when the switcher opened. The reactor records it when it handles `open_context_switcher`, and the switcher passes its id with each command, so the panel taking key focus can't change it.

The create and edit views list the windows on screen (the focused screen in `per_screen` scope) and the context's current members, each with a checkbox. Unchecking a member whose window is gone removes its record (`Contexts::remove_record`, R23).

The panel is an `NSPanel` with the `nonactivatingPanel` and borderless styles, at floating level. It joins all Spaces and overrides `canBecomeKey` to return true. That lets the user type without making Sugarglider the frontmost app. Rooms' `PalettePanel` does the same. It appears centered on the focused screen.

The JSON contract `docs/specs/contexts-switcher-contract.md` fixes the payload and the panel's keys and views: Esc goes back from the naming, rename, create, and edit views and closes from the list; ⌘N opens a naming step that holds the typed query; a second show toggles the panel closed; the create and edit views show a pinned window checked and fixed, and create gives it no record; a native tab group is one row, with its `tab_count`; and the target window is null while the focused window is untracked, parked, or Sugarglider's own.

### Menu bar

- The status item shows the name of the active context. In `per_screen` scope it shows the focused screen's context. It shows nothing extra when Everything is active. If `experimental.status_icon.space_index` is on, it shows both, for example "2 · Comms". A context name longer than 20 characters is cut to 20 and an ellipsis is added (`src/actor/status.rs`, `short_name` and `MAX_STATUS_NAME`).
- The menu gains a section with each context (a checkmark on the active one, and the actual key of a `switch_context` binding for that context as the key equivalent when one exists, not the context's number), "Show Everything", "New Context from Current Windows…", a "Send Window to" submenu, and "Open Switcher…".

### Key bindings

New commands (all are `snake_case` in TOML):

| Command | Argument | Does |
|---|---|---|
| `open_context_switcher` | none | Opens the switcher |
| `switch_context` | number 1–9 or name | Switches (R12) |
| `show_everything` | none | R27 |
| `previous_context` | none | R18 |
| `add_window_to_context` | number or name | Adds the focused window (R37) |
| `move_window_to_context` | number or name | Moves the focused window out of the active context and into the named one (R37) |
| `remove_window_from_context` | none | Removes the focused window from the active context (R37) |
| `toggle_window_pinned` | none | R3 |

The default config ships these bindings commented out while the feature is experimental. The hotkey event tap swallows a bound key even when the feature is off, so shipping them active would take ⌃⌥Space away from people who don't use contexts. macOS also binds ⌃⌥Space to "Select next source in Input menu" by default. Sugarglider's event tap would take the key first; the user docs must mention this.

```toml
# "Ctrl + Alt + Space" = "open_context_switcher"
# "Ctrl + Alt + 0" = "show_everything"
# "Ctrl + Alt + 1" = { switch_context = 1 }
# "Ctrl + Alt + 2" = { switch_context = 2 }
# ... through 9
# "Ctrl + Alt + Tab" = "previous_context"
```

### Command line

```
sugarglider context list [--json]
sugarglider context current [--json]
sugarglider context switch <query>
sugarglider context everything
sugarglider context previous
sugarglider context create <name>        # a new context from the windows on screen
sugarglider context add <query>          # adds the focused window
sugarglider context move <query>         # moves the focused window
sugarglider context remove               # removes the focused window from the active context
sugarglider context rename <query> <new name>
sugarglider context delete <query>
sugarglider context number <query> <1-9>
sugarglider context pin
sugarglider context forget <query> <record>
```

- `<query>` is a number or a name. The reactor resolves it with `model::contexts::resolve`: a number from 1 to 9 or an id names that context, and any other name matches exactly or by the switcher's ranking among the named contexts. A name made of digits, such as "2024", names the context by its number unless `--name` says it is a name. Everything and Unsorted match only their exact names, and Unsorted only while it is listed, so an empty Unsorted is never offered (I3).
- `create` makes a context whose members are the tracked windows on the visible Spaces. Parked windows are left out, and a pinned window gets no record, because it is a member of every context already (R3). The new context becomes the active context.
- `pin` pins or unpins the focused window. `forget` removes a gone window's member record from a named context. Pass the context as `<query>` and the zero-based `record` value from that context's `members` array in `list --json`. An open window's record cannot be forgotten; remove its window instead. The command reads the current snapshot first, then the reactor checks the record's app and title again before removing it. If the list changed, list the contexts again and use the new index.
- Human-readable output goes to stdout. `--json` prints the snapshot shape below, whose top-level `active` names the active context.
- On failure (no match, server not running, feature off) the command prints the reason to stderr and exits with status 1. A switch or a new context while no screen shows a managed Space, as when Sugarglider is paused or at the login window, fails with "No Space is managed right now". A command that changes something waits about a second for the reactor's result (I3), and exits 1 when the reactor reports a failure or doesn't confirm in time.

`sugarglider context list --json`:

```json
{
  "scope": "global",
  "active": "Comms",
  "screens": [{ "id": 1, "active": "Comms" }],
  "contexts": [
    { "name": "Comms", "number": 1, "active": true, "apps": ["WhatsApp", "Microsoft Teams"], "windows": 2,
      "members": [
        { "record": 0, "app": "WhatsApp", "title": "WhatsApp", "window": { "pid": 903, "idx": 9201 } },
        { "record": 1, "app": "Microsoft Teams", "title": "Team", "window": { "pid": 904, "idx": 9202 } },
        { "record": 2, "app": "Mail", "title": "Inbox", "window": null }
      ] },
    { "name": "Relax", "number": 2, "active": false, "apps": ["WhatsApp", "Google Chrome"], "windows": 2, "members": [] }
  ],
  "unsorted": 3
}
```

### Raycast and SuperCmd

A Raycast [script command](https://github.com/raycast/script-commands) needs no published extension. Ship it as `contrib/raycast/switch-context.sh`:

```bash
#!/bin/bash
# @raycast.schemaVersion 1
# @raycast.title Switch Context
# @raycast.mode compact
# @raycast.packageName Sugarglider
# @raycast.argument1 { "type": "text", "placeholder": "Context" }
set -euo pipefail
/usr/local/bin/sugarglider context switch "$1" 2>&1
```

- `make install` puts the binary in `/usr/local/bin`. Raycast adds that directory to `PATH`, but the absolute path avoids depending on that.
- In `compact` mode Raycast shows the last output line, and treats a non-zero exit as a failure. `2>&1` makes the error text visible.
- SuperCmd's documentation says it imports Raycast script-command folders and runs most Raycast extensions. Nobody has tested this with Sugarglider yet.
- A full Raycast extension with a searchable list is a later item.

### Preferences

The Preferences window has a "Contexts (experimental)" switch and a scope picker. The picker sends `contextsScope` through `PreferencesJson`; `write_preferences_to_file` (`src/config.rs`) writes it as `settings.experimental.contexts.scope`. An older payload without `contextsScope` preserves the configured scope.

## Design

### Overview

```
hotkey · menu · switcher (FFI) · CLI (IPC)
                 │  ContextCommand
                 ▼
WmController ─► SpaceManager ─► Reactor ─► LayoutManager
                                  │          ├─ one layout per (Space, context)
                                  │          └─ activates a context's layout and
                                  │             reconciles its member nodes
                                  ├─ owns model::contexts::Contexts
                                  ├─ builds SwitchInput, calls plan_switch
                                  ├─► journal ~/.glide/parked.json   (written first)
                                  ├─► app threads: SetWindowFrame, Raise
                                  ├─► ~/.glide/contexts.json
                                  └─► snapshot ─► CLI, switcher, menu bar
```

### Model: `src/model/contexts.rs`

The model is pure, like the rest of `src/model/`. It does no I/O and reads no clock. Most-recently-used order is a sequence number that increments on each switch, not a timestamp.

Types:

```rust
pub struct ContextId(u32);
pub enum ContextKey { Everything, Unsorted, Named(ContextId) }

pub enum RecordLink { Empty, Live(WindowId), Pending(WindowId) } // R23

pub struct MemberRecord {
    pub bundle_id: Option<String>,
    pub app_name: Option<String>,
    pub title: String,
    pub window_server_id: Option<WindowServerId>,
    pub link: RecordLink, // not persisted
}

pub struct Context {
    pub id: ContextId,
    pub name: String,
    pub number: Option<u8>,
    pub members: Vec<MemberRecord>,
    pub last_used: u64,
}

pub struct Contexts {
    contexts: Vec<Context>,
    pinned: Vec<MemberRecord>,
    active: ContextKey, // global scope; M9 keeps one per screen
    previous: Option<ContextKey>, // never equals active (R18)
    next_id: u32,
    use_seq: u64,
    // and the use numbers of Everything and Unsorted, and focus order
}
```

The reactor owns `Contexts`, so the active context lives in the reactor's state, and `contexts.json` persists it. `LayoutManager` gets the active context as an argument (L1).

Pure functions, each with unit tests:

- `rank(query, &Contexts, unsorted_has_windows) -> Vec<(ContextKey, NameMatch)>` ranks the switcher's entries.
- `resolve(query, &Contexts, lists_unsorted) -> Result<ContextKey, ContextError>` names the entry a command's query names: a number or an id, an exact name, or the best match of the ranking among the named contexts. Everything and Unsorted match only their exact names, and Unsorted only while `lists_unsorted` and a context exists (R28, R29). The reactor resolves every context command with it.
- `match_windows(windows, &Contexts, pass) -> Vec<Vec<RecordMatch>>` runs R22. `pass` is `MatchPass::Arrival` or `MatchPass::Switch { target }`. `match_window` does the same for one window.
- `plan_switch(&SwitchInput) -> SwitchPlan` plans a switch (R12 to R15).
  - `SwitchInput` lists each visible screen with its Space, its active context after the switch, and its windows. Each window carries its contexts, when it last took focus, and whether it is pinned, untracked, Sugarglider's own, in the reactor's visible-window set, or already parked.
  - `SwitchPlan` lists windows to park, windows to put back, windows to move to another screen, and the window to focus.
  - The reactor builds `SwitchInput` from its own state (visible windows, frames, screens, and the parked set) and calls `plan_switch`. When R24 started the switch, the reactor focuses the window the user focused instead of the plan's choice (R12, step 5).

Membership changes are methods on `Contexts`. `windows_appeared` handles new windows (R20, R21), `rejoin_all` handles windows found at launch (R38) and matching during a switch, and `title_changed`, `window_closed`, `app_terminated`, `app_still_running`, `forget_window_server_ids`, and `remove_record` keep the records current (R22, R23).

The layering rule allows `model` to use `sys` for geometry types only. Pass screens as plain indexes or keep screen ids in the actor.

### Layouts

Facts from the current code:

- `LayoutManager.layout_mapping: HashMap<SpaceId, SpaceLayoutMapping>` holds each Space's layouts. All layouts share one `LayoutTree` (`src/actor/layout.rs`, struct `LayoutManager`).
- `LayoutManager::try_layout(space)` returns the active layout of the Space's `layout_mapping` entry, and `layout(space)` unwraps it (`src/actor/layout.rs`, `LayoutManager::layout`). Several paths use `layout_mapping` directly instead. `handle_command` takes the mapping with `layout_mapping.get_mut`, calls `prepare_modify`, and runs every command on `mapping.active_layout()`. `NextLayout`, `PrevLayout`, and `ChangeLayoutKind` change that mapping. `ensure_layout_kind_allowed_for_space` and `convert_active_scroll_layouts_to_tree` walk `layout_mapping`, and the `SpaceExposed` handler inserts into it and calls `activate_size` on it.
- One window can have a node in several layouts at once. `window_nodes: BTreeMap<WindowId, Vec<NodeId>>` in `src/model/window.rs` documents this, and `LayoutTree::window_node(layout, wid)` picks the node for one layout. `remove_window_from(layout, wid)` removes it from one layout; `remove_window(wid)` removes it from all.
- Each `SpaceLayoutMapping` counts references to its own layouts and frees them inside `activate_size` (`src/model/layout_mapping.rs`). Nothing else frees layouts. `SpaceLayoutMapping::new` always creates a new layout.
- Several paths remove a window from every layout. `WindowRemoved` calls `remove_window`, `AppClosed` calls `remove_windows_for_app`, and `AppsRunningUpdated` calls `retain_apps`. `convert_layout_kind` and the `ToggleWindowFloating` command also call `remove_window`. `WindowAdded`, and the added side of `WindowSpaceChanged`, add a window only to the active layout. The removed side of `WindowSpaceChanged` removes it only from the active layout of the Space it left.
- **When a window stops being visible, Sugarglider removes it from the active layout.** `set_windows_for_app` detaches its node. When the window is visible again, it is added back as the last child of the root, and its old place is lost (`src/model/layout_tree.rs:320-359`). Across visibility changes, layouts that are not active keep their nodes.

This last fact is the main constraint. If a switch made windows invisible, every switch would throw away the layout of every hidden window. So the design separates membership from visibility:

- **L1.** Add `context_layouts: HashMap<(SpaceId, ContextKey), SpaceLayoutMapping>` to `LayoutManager`, with `#[serde(default)]`. `Everything` keeps using `layout_mapping`. The reactor passes each Space's active context to `LayoutManager` as `SpaceExposed(SpaceId, CGSize, ActiveContext)`. `ActiveContext { key, members }` holds the context's key and the open windows that are its members. `LayoutManager` reads `members` only when the context gets its first layout on the Space (L3). For Everything, the reactor passes `ActiveContext::EVERYTHING`. `LayoutManager` keeps the key only in a `#[serde(skip)]` field (`active_contexts`), so `layout.ron` never stores which context is active. The accessors `active_mapping(space)` and `active_mapping_mut(space)` return the mapping the Space shows (`shown_context`). That is the active context's mapping, or the Space's `layout_mapping` entry under Everything. `try_layout`, `handle_command`, `ChangeLayoutKind`, `NextLayout`, and `PrevLayout` use them. The `SpaceExposed` handler reaches the active context's mapping through `mapping_mut(space, key)`. `ensure_layout_kind_allowed(space, key)` converts one context's mapping, and `convert_active_scroll_layouts_to_tree` converts every mapping, the context mappings included.
- **L2.** Each context mapping keeps the per-screen-size memory that `SpaceLayoutMapping` provides today. While a context is active on a Space, `NextLayout` and `PrevLayout` do nothing there.
  - The `SpaceExposed` handler calls `activate_size` on the mapping of the Space's active context. If that mapping is missing, it creates it first (L3).
  - Inside `SpaceChanged` and `ScreenParametersChanged`, the reactor resolves each visible Space's context, has `LayoutManager` create the mapping if it is missing, activates its size, and applies the context again, all in the same event. This re-apply is not a switch command, so it doesn't end the grace that R37 gives a window added since the last switch.
  - When a context becomes active on a Space, Sugarglider calls `activate_size` on the context's mapping with the Space's current size. `layout_mapping` already holds that size.
  - If the active context's mapping for a Space is missing anyway, `layout(space)` falls back to Everything's layout and logs an `error!` (`shown_context`).
  - Deleting a context (R6) calls `LayoutManager::remove_context_layouts`, which calls `remove_layout` on every layout in every mapping of that context. At load, once the reactor has read `contexts.json`, it calls `LayoutManager::retain_context_layouts` to drop the `context_layouts` entries of named contexts that no longer exist. Unsorted's layouts stay.
- **L3.** When a context gets its first layout on a Space, `create_context_mapping` clones the layout the Space shows (`LayoutTree::clone_layout`) and removes the windows that aren't in `ActiveContext.members`. The layout it clones can be Everything's or another context's. Creating a context keeps the arrangement the user sees. A new constructor, `SpaceLayoutMapping::from_layout(size, layout)`, takes the clone with a reference count of 1.
- **L4.** When a context layout is active, `WindowsOnScreenUpdated` and `WindowAdded` only add windows that are members. A visible window that isn't a member, and isn't parked yet, never gets a tile. The reactor applies this filter in H2's predicate.
- **L5.** The design requires parked windows to remain in the WindowServer visible-window list. H1 leaves a bottom strip. A one-display TextEdit probe confirmed `OnScreenOnly` listed that window; other apps and displays remain untested (Q1). When WindowServer lists a parked window, H2 passes its frame from before parking to `set_windows_for_app`, so parking does not remove its node. A switch changes the active layout and the reactor's parked set in one reactor event. No visibility update can see a half-finished switch.
- **L6.** Members that the user minimized, or whose app the user hid, leave the active layout as they do today. They stay members.
- **L7.** `floating_windows`, `floating_restore_frames`, and size locks are keyed by window. In the first version, a floating window shared by two contexts has the same frame in both.
- **L8.** Reconcile. After a switch activates C's mapping, in the same reactor event, the reactor:
  1. sends each app's visible windows to the layout again (`send_visible_windows_to_layout`, through H2's predicate), so `set_windows_for_app` adds the members that have no node in C's layout. The members about to be put back still count as parked here, so the layout sees them at their frames from before parking and never as tab groups that share a corner. The windows it re-adds are ordered by their first frames (`reorder_columns_by_position`), so a member returns to its old column;
  2. clears the parked state of every member of C on the visible Spaces, and queues each window the layout doesn't place, a floating member, to go back at its frame from before parking (H3);
  3. calls `update_layout(&[], true)`.

  R27 and R32 run the same reconcile for Everything. A member can lack a node in C's layout when its app relaunched while another context was active, when it joined C while C wasn't active, when it was pinned, or when C's layout was cloned from a layout without it.
- **L9.** Changing the layout kind and floating a window keep the window's nodes in other contexts' layouts.
  - `convert_layout_kind` removes each window only from the layout it converts, with `remove_window_from(layout, wid)`.
  - The `ToggleWindowFloating` command removes the window from every layout of the context the Space shows, on every Space and for every screen size (`remove_window_from_shown_context`). Under Everything, those are all the layouts in `layout_mapping`, so floating works as it does today for people without contexts. Without a Space, the command removes the window from every layout.
  - Unfloating a window reuses its node when the layout still has one.
  - Today `convert_layout_kind` also removes the windows from the Space's layouts for other screen sizes. With L9 those layouts keep them. This changes behavior for people who never create a context, but only the experimental scroll layouts can change a layout's kind. This is accepted.

  A window that is floating when D becomes active still loses its node in D at the reconcile (L8), because `WindowsOnScreenUpdated` leaves floating windows out of `set_windows_for_app` and floating is per window (L7).
- **L10.** When a window leaves a Space while contexts are in use (`contexts_in_use`: the flag is on and any of these holds: a context exists, the active context isn't Everything, or a window is parked), Sugarglider removes it from every context mapping of that Space and from that Space's `layout_mapping` entry, for every screen size. Otherwise only the layout the Space shows loses it, as today. The reconcile (L8) adds it on the new Space when a context there needs it.
- **L11.** `WindowAdded` does nothing when the window already has a node in the target layout.

### Parking

Facts from the current code:

- `SetWindowFrame` is the only request parking needs. The app requests are `Terminate`, `GetVisibleWindows`, `SetWindowFrame`, `AnimationFrame`, `BeginWindowAnimation`, `EndWindowAnimation`, `Raise`, and `WindowDestroyed` (`src/actor/app.rs`, enum `Request`).
- A window whose frame is on no screen belongs to no Space and drops out of the layout at the next refresh (`best_screen_idx_for_window`, test `windows_parked_off_screen_belong_to_no_screen`). A window with 1 pixel on screen does belong to that screen.
- `ScreenInfo` (`src/sys/screen.rs`) and the reactor's `Screen` (`src/actor/reactor.rs`) carry each display's visible frame and full bounds.
- A `SetWindowFrame` echo comes back as `WindowFrameChanged` with `requested` set. The reactor handles only minimum sizes there and returns without touching `frame_monotonic` (`Reactor::handle_event`, `Event::WindowFrameChanged`). `update_layout` skips a window whose target equals `frame_monotonic`. It stops writing a target after `MAX_FRAME_ATTEMPTS` (5) tries within `FRAME_ATTEMPT_RESET` (2 seconds).
- The reactor treats windows of one app that are on screen and share the same rounded frame (`Reactor::frame_key`, `window_on_screen`) as tabs; a parked window and a window that isn't on screen are no tab. `WindowBecameVisible` skips a window that such a sibling covers (`dominated_by_existing`). `WindowDestroyed` skips `WindowRemoved` when a sibling has the same frame (`dominated_by_sibling`). `send_visible_windows_to_layout` keeps one window per frame.
- Floating windows live outside the tree, in `LayoutManager.floating_windows`. `update_layout` writes only the frames that `calculate_layout_and_groups` returns, plus the one-shot `pending_frame_overrides`.

Design:

- **H1.** Production parking chooses the first clear bottom corner of the window's display, right then left, using the display's full bounds (`bounded_bottom_corner`). The requested frame leaves one point on that display and has no overlap with any other display's full bounds. The app actor reads the frame back from AX after the write. The reactor accepts that readback only for the current window-server ID and transaction, with the same window size within `SameAs` tolerance, no overlap with another display, and a strip touching the selected bottom corner that is wider than zero and at most 1 point wide and 32 points tall (`accepted_bottom_strip`). A top corner, an overlapping display, an unconfirmed write, or a wider strip fails. The context returns to Everything and the original journal frame remains available for restoration. `devtool park` uses this automatic choice and acceptance rule; an explicit `--corner` is a diagnostic probe.
- **H2.** The reactor keeps each parked window's original frame, selected screen and corner, current window-server ID, observed frame, and awaited write. A parked window's Space is the Space of its original frame, not of the corner it sits in. One predicate, `reaches_layout(space, wid)`, decides whether a window may reach the layout: it rejects parked windows, whatever their geometry says, and applies L4's member filter. Every path that sends a layout event for a window applies it: `WindowBecameVisible` (which sends `WindowAdded`), frame changes, `MouseMovedOverWindow`, and `send_visible_windows_to_layout` (the window-server snapshot). A frame change of a parked window stays out of the layout; it updates the observed frame and can trigger a repark. An accepted bottom strip needs no repark. Before a batch repark, the reactor checks every target, retry cap, and journal entry. If one fails, it writes no new journal entries or repark frames and restores Everything. A refused readback also restores Everything (R39).
- **H3.** Putting a member back needs no special request. The reconcile (L8) gives the member a node in the active layout, so `update_layout` writes its frame. The reactor clears the member's parked state before that write, and R31 removes the journal entry when the echo confirms the frame. A member the layout doesn't place, a floating member or one with no node, has no tile to return to, so the reactor puts it back at its frame from before parking through `pending_frame_overrides`. Showing Everything puts non-members back at their journal frames (R27).
- **H4.** Park and unpark writes go through the reactor's frame bookkeeping. Each write takes a new transaction id (`WindowState::next_txid`), so the reactor ignores stale frame reads from before it. Parking sets `frame_monotonic` to the parked frame, so `update_layout` doesn't skip the unpark write as unchanged. Parking and unparking both clear the window's `frame_attempts` entry, so quick switches never reach `MAX_FRAME_ATTEMPTS`.
- **H5.** Parked windows retain their actual accepted AX frame, which may be a bottom strip up to 1 by 32 points. The tab checks skip parked windows. `dominated_by_existing`, `dominated_by_sibling`, and the frame grouping in `send_visible_windows_to_layout` never treat a parked window as a sibling tab.

A switch writes one frame per window that it parks or puts back. Q5 measures what that costs.

### Journal and state files

Both files live in `data_dir()` (`~/.glide`, `src/config.rs`), next to `layout.ron`. Both are JSON, because people and scripts read them. Both are written atomically with `tempfile::NamedTempFile::new_in(dir)` and `persist`, like `write_preferences_to_file`. (`LayoutManager::save` is not atomic; don't copy it.) Their paths are injectable, so tests use a temporary directory.

`~/.glide/parked.json`:

```json
{
  "version": 1,
  "entries": [
    { "pid": 812, "bundle_id": "com.google.Chrome", "window_server_id": 9123,
      "title": "Docs", "frame": { "x": 0, "y": 25, "w": 1440, "h": 875 } }
  ]
}
```

- An entry is written before its window is parked (R30). It is removed when R31 confirms the window is back, or when the window or its app is gone.

`~/.glide/contexts.json`:

```json
{
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
  "active": { "global": 1 },
  "boot_id": "1790000000.000001"
}
```

- The reactor writes `contexts.json` after every change to contexts or membership, after each switch, when an app that has members quits, and on quit. A title change alone doesn't write the file, because terminals and browsers change titles constantly. The app-quit write keeps the last titles that R22 needs to match relaunched windows.
- `active` is `{ "global": <key> }` or `{ "per_screen": { "<display id>": <key> } }`. `<key>` is a context id or one of the strings `"everything"` and `"unsorted"`. A missing or unknown value, or an id that no context has, loads as Everything. In `per_screen` scope, each screen gets its saved context, and a screen without an entry shows Everything. On a cold change to `global`, R11 selects the focused screen's saved context and clears the per-screen map before another switch can save. R32 doesn't change `active`.
- `boot_id` names the boot that wrote the file: the boot session UUID from `kern.bootsessionuuid`, or the time the Mac booted from `kern.boottime` when that can't be read. Window server ids are valid within one boot, so when the file comes from another boot, the reactor forgets every saved window server id at load (R22). The reactor reads `contexts.json` only while the flag is on.
- A value that breaks a rule is repaired at load, not rejected: a number outside 1 to 9 or one that another context holds is dropped; a context whose id an earlier context has takes a fresh id and keeps its members; an empty, reserved, or taken name gets a number (R4); and the id and use counters are raised above every value the file holds. Only another version, or a file that isn't this shape, fails to load, and then the file is moved aside like the journal and Sugarglider starts with no contexts.
- The files contain window titles. The user docs must say so, as Rooms does.
- Context layouts are part of `LayoutManager`, so they survive `save_and_exit` plus `--restore` like today's layouts. After a crash or a reboot, membership survives in `contexts.json`, but layouts start fresh from member order. Membership and the active context are never in `layout.ron`.

### Commands and dispatch

- Add `reactor::Command::Context(ContextCommand)` (`src/actor/reactor.rs`, enum `Command`). `Command` is untagged, so the variant names must not collide with `LayoutCommand` names.
- `open_context_switcher` is a reactor command. The reactor knows the target window (its main window at that instant) and the windows on screen with their titles. It builds the switcher's JSON and calls `sugarglider_show_context_switcher` from its own thread. The reactor already calls the Swift bridge from its thread (`swift_bridge::hide_drop_zones` in the `ReactorCommand::Debug` handler), and the Swift side moves the work to the main queue.
- `switch_context` takes a `ContextRef`, which is a number, a name, or an id. A bare integer is always a number from 1 to 9. An id is tagged, `{ id = 7 }` in TOML and `id(7)` in RON, because `Command` and `WmCommand` are `#[serde(untagged)]` and an untagged id would read as a number; the older spelling `Id(7)` is read too. A command from a key binding or the command line carries the query as the user wrote it, and the reactor resolves it against its own state when it runs the command (I3). The switcher sends the id or the built-in name it read from the snapshot.
- New commands must be added to the exhaustive matches in `WmController::handle_event`, `Reactor::handle_event`, and `describe_command` in `src/ui/preferences_json.rs`. New reactor `Event` variants, such as the title change (R22) and the event of R33, must also be added to `MainWindowTracker::handle_event` (`src/actor/reactor/main_window.rs`).
- The reactor carries out a switch. It builds `SwitchInput` from its own state, calls `plan_switch`, and runs R12 in order, journal first. `LayoutManager` activates the context's mapping on each Space and reconciles member nodes (L8). It never sees the plan.

### IPC

Facts from the current code:

- Requests and responses are RON over a `CFMessagePort` named `org.glidewm.server`. The exchange is synchronous; the client waits up to 1000 ms (`src/bin/glide.rs`).
- `Response` carried only `Pong`, `Success`, and `Error` before contexts and couldn't carry data; I2 adds the snapshot and the pending reply.
- The port callback runs on the main thread, and so does `WmController`. The reactor runs on its own thread. So the callback can't wait for a reply from the reactor; that would block the thread that must forward the request.

Design:

- **I1.** The reactor publishes a `ContextsSnapshot` into a global `OnceLock<RwLock<Arc<ContextsSnapshot>>>` after every change. `CURRENT_CONFIG` in `src/ui/swift_bridge.rs` is the precedent. The snapshot also carries the results of the recent commands, each under the request id its client picked.
- **I2.** Add `Request::Context(ContextRequest)` with `List`, `Current`, `Run(RequestId, ContextCommand)`, and `Result(RequestId)`, and `Response::Contexts(ContextsSnapshot)` and `Response::Pending`.
- **I3.** `List` and `Current` read the snapshot and reply at once. `Run` replies `Success` at once and sends the command, with the client's request id, through `wm_tx` to the reactor. The reactor resolves the query against its own state with the single model resolver (`model::contexts::resolve`) and publishes its result in the snapshot: no error when the command ran, or the reason it did nothing. The client polls `Result(id)` until the result is there or about a second passes, and exits 1 on a failure or when nothing confirms the command. Because the command is resolved when the reactor runs it, `sugarglider context create X && sugarglider context switch X` works even though the snapshot lags.
- **I4.** An older server can't parse the new request and replies with nothing. Then the client prints "The running Sugarglider doesn't support this command. Restart it." instead of "Deserializing response failed".

### Swift bridge

- Rust to Swift: add `sugarglider_show_context_switcher(json)` and `sugarglider_hide_context_switcher()`. The reactor calls them (see Commands and dispatch). The JSON is the contract's show payload: the snapshot, the target window, and the windows on screen with their titles and app names. A show while the panel is open closes it, so the hotkey toggles. Rust never tracks whether the panel is open; Swift decides.
- Swift to Rust: add `sugarglider_rank_contexts(query)` (returns JSON, freed with `sugarglider_free_string`) and `sugarglider_run_context_command(json)`. `rank_contexts` returns `model::contexts::rank` over the published snapshot, or NULL while contexts are unavailable; it replaces the `sugarglider_get_contexts()` of the earlier draft. `run_context_command` checks the command against the same snapshot and returns an error for invalid JSON, an unknown command, the flag off, an unknown context id, a create or rename name that is empty, reserved, or taken (R4), a `set_number` outside 1 to 9, or a stale `remove_records` item. Otherwise it returns NULL after sending the command. Only the commands that act on a window, `add_window`, `move_window`, and `toggle_pinned`, carry one, the target window. Neither function waits on the reactor or on `WmController`.
- The published snapshot carries what `rank` and `run` need: each context's id, name, use number, and member records, with the app name a record derives and whether it has an open window; the use numbers of Unsorted and Everything; and the unsorted count that leaves pinned windows out (R3). The show payload lists one row per native tab group, with its `tab_count`. The contract `docs/specs/contexts-switcher-contract.md` fixes the payload, the commands, the rank result, and the error messages.
- `sugarglider_run_context_command` sends the command through the sender that `CONFIG_UPDATE_SENDER` (`src/ui/swift_bridge.rs`) already holds, a `wm_controller::Sender`. Rename that global if its name gets in the way; don't add a second one. The reactor hides the panel after a context command succeeds, whatever sent it, when the feature turns off, and when an exit is pending (R32, R33).
- Each new Swift-to-Rust symbol needs a `-Wl,-u,_<symbol>` line in `build.rs` and a `black_box` reference in `swift_bridge::init`. Commit `73ae968` explains why.

### Config

```toml
[settings.experimental.contexts]
# Named window sets you switch between. See docs/specs/contexts.md.
enable = false
# "global": a switch changes every screen. "per_screen": only the focused screen.
scope = "global"
```

- Declare the struct with `#[derive(PartialConfig!)]` inside `Experimental` (`src/config.rs`). Every field needs a value in `sugarglider.default.toml`; the tests `default_config_is_valid` and `default_settings_match_unspecified_setting_values` check this.
- `enable` arrived in M5a and `scope` in M9.
- On a config reload that sets `enable = false`, Sugarglider shows Everything (R33). A reload that changes `scope` applies R11.

## Implementation traps

- The test reactor never receives raises at the app handles. `Reactor::new` drops the raise manager's receiver (`let (raise_manager_tx, _rx)`), and layout raises go to `raise_manager_tx`, so a switch never reaches the harness's `Request::Raise(..) => todo!()` (`src/actor/reactor/testing.rs`, `simulate_events_for_requests`). A focus test must replace `reactor.raise_manager_tx` with a channel it reads, as `it_surfaces_on_screen_change_when_the_snapshot_is_empty` does. It must then send the `ApplicationActivated` and `ApplicationMainWindowChanged` events itself, with the right `Quiet`. `Apps` needs a `Request::Raise` implementation only for a test that sends raises straight to app handles.
- A test reactor replays its own recording when it is dropped (`testing.rs`, `impl Drop`). Every new reactor `Event` must survive a serde round trip and must be added to `MainWindowTracker::handle_event`.
- New `LayoutManager` fields need `#[serde(default)]`. Bless `tests/snapshots/current.ron` with `GLIDE_BLESS_SNAPSHOTS=1`, and keep the old snapshots restoring.
- `Status::update_space` sets the status item title on every Space change (`src/actor/status.rs`). It would overwrite the context name; merge the two.
- The status menu is built once and has no dynamic items (`src/ui/status_bar.rs`). The context list needs an `NSMenuDelegate` or a rebuild on change.
- `notification_center.rs` panics on any notification name it doesn't know. If you observe new NSWorkspace notifications, add a branch.
- Two spellings of the same hotkey (`"Alt + Ctrl + 1"` and `"Ctrl + Alt + 1"`) both survive the config merge, and the second registration panics (`src/sys/event.rs`, `register_wm` calls `unwrap`).
- A new window reaches the layout twice. For `WindowCreated`, the window-server actor sends `WindowCreated`, then `WindowsOnScreenUpdated`, then `WindowBecameVisible` (`src/actor/window_server.rs`), and `WindowAdded` calls `add_window_after` without checking for a node. R38 and L11 cover this.
- `ReactorCommand::SaveAndExit` calls `process::exit` in the same match arm, on the reactor thread. Frame writes are asynchronous requests to app threads in the same process. R32 must complete, not only start, before that call.
- There are no signal handlers. After `SIGTERM` or `kill -9`, only the journal (R34) protects windows.
- `CONTRIBUTING.md` describes the log target as `glide_wm::…`; in this crate it is `sugarglider::…`.

## What we don't know yet

These are facts about macOS that the code can't answer. Q1 has partial one-display evidence below. The M1 physical spike remains open for Q1 to Q4, and Q5 still needs a measured target.

- **Q1.** Does macOS keep a window within H1's bottom-strip bound? Check other apps and two displays, including whether WindowServer `OnScreenOnly` lists each parked window. With two displays showing different Spaces, check whether AX lists both displays' visible windows; omitting one can make the safety check show Everything. Check whether Mission Control shows a parked window, whether it stays parked when its app is activated, and whether apps move their own parked windows back.
  - Partial physical evidence, 2026-09-26: Tim tested TextEdit (PID 33920, window-server ID 47821) on one display with Sugarglider stopped. The visible screen frame was `(0, 33, 1512, 949)`, and the window started at `(305, 367, 586, 488)`. Each probe made three attempts. AX returned the same observed frame on all three. The old devtool's exact-frame check reported an error, and Tim manually restored the window to exactly `(305, 367, 586, 488)` after each probe.

    | Probe | Requested frame | AX observed frame | Visible overlap |
    | --- | --- | --- | --- |
    | Bottom right | `(1511, 981, 586, 488)` | `(1511, 950, 586, 488)` | `1 × 32` points |
    | Bottom left | `(-585, 981, 586, 488)` | `(-585, 950, 586, 488)` | `1 × 32` points |
    | Top right | `(1511, -454, 586, 488)` | `(1511, 33, 586, 488)` | `1 × 488` points |
    | Top left | `(-585, -454, 586, 488)` | `(-585, 33, 586, 488)` | `1 × 488` points |
    | Horizontal edge | `(1512, 950, 586, 488)` | `(1472, 950, 586, 488)` | `40 × 32` points |

    A position-only bottom-right probe requested `(1511, 981, 586, 488)` and again read `(1511, 950, 586, 488)` on all three attempts. AX list confirmed the result, and Tim restored `(305, 367, 586, 488)` exactly. Tim accepted a verified bottom-corner strip up to `1 × 32` points as the production bound. These probes did not check WindowServer `OnScreenOnly` or Mission Control. Other apps, two displays, activation, and app-driven moves also remain open.

  - Bounded-rule follow-up, 2026-09-27: With Sugarglider stopped, Tim ran the rebuilt default `devtool park` on the same TextEdit window from `(305, 367, 586, 488)`. It requested `(1511, 981, 586, 488)` and accepted the AX frame `(1511, 950, 586, 488)` on attempt 1. `devtool window-server list`, which uses `OnScreenOnly`, listed window-server ID 47821, PID 33920, layer 0, at that observed frame. `devtool list ax` agreed. An intervening manual restore command specified height 48 by mistake; TextEdit reported height 82. A second default park from `(305, 367, 586, 82)` also accepted a `1 × 32` strip, and the corrected restore returned exactly to `(305, 367, 586, 488)`. This confirms bounded parking and WindowServer presence for this one window on one display. Other apps, activation, two displays, and live context switching remain untested.
  - Space-scoping probe, 2026-09-27: Tim left TextEdit window-server ID 47821 on its original macOS Space and switched to another Space. The rebuilt `devtool --bundle com.apple.TextEdit list ax` reported `0 windows` in 83.94 ms, and `devtool window-server list` with `OnScreenOnly` had no row for that ID. On returning to TextEdit's Space, both AX and `OnScreenOnly` listed ID 47821 again at `(305, 367, 586, 488)`. This is one window on one display. It does not establish how other apps or displays behave, or explain an earlier no-row AX result for which WindowServer still listed the window.
  - Mission Control probe, 2026-09-27: With Sugarglider stopped on TextEdit's original Space, default `devtool park 33920 47821` accepted `(1511, 950, 586, 488)` under the `1 × 32` bound. Tim opened Mission Control and saw TextEdit's thumbnail. He then selected it and reported that the window moved somewhere he could not see; it did not become a usable desktop window. An immediate `set-frame 33920 47821 305 367 586 488` could not find a matching window. Targeted AX listing reported zero TextEdit windows, and WindowServer `OnScreenOnly` had no row for ID 47821. After `open -a TextEdit`, AX listed the same window at its parked frame, and `set-frame` restored `(305, 367, 586, 488)` exactly on attempt 1. This one-app, one-display probe confirms thumbnail presence but not recovery by selecting it. It does not show whether the window changed Space, whether other apps behave the same way, or whether a user could recover it without the diagnostic tool.
  - Native recovery probe, 2026-09-27: With Sugarglider stopped, Tim parked TextEdit PID 33920, window-server ID 47821, at the accepted AX frame `(1511, 950, 586, 488)`. `open -a TextEdit` left AX reporting that parked frame. Tim chose TextEdit Window > Zoom and reported that the window stayed offscreen. Entering Full Screen made it usable. A later targeted AX listing showed the normal window with the same ID and size at `(926, 494, 586, 488)`. `devtool set-frame` restored `(305, 367, 586, 488)` exactly on attempt 1. This establishes one macOS-native recovery route for this TextEdit window on one display. It does not establish the route for other apps, windows, or displays, or show whether a user can find the bottom strip without another app action.
  - Calculator refusal, 2026-09-27: With Sugarglider stopped, Tim verified Calculator was not running and opened a fresh instance. Its standard, nonresizable AX window had PID 24326, window-server ID 61062, and frame `(758, 197, 230, 408)`. Default `devtool park 24326 61062` requested the production bottom-right frame `(1511, 981, 230, 408)` and returned `kAXErrorFailure` on the write. A targeted AX list still showed the original frame. Tim quit only that new Calculator instance and confirmed no Calculator process remained. This is a parking refusal, not a recovery failure: the window did not move. Production's missing-echo deadline returns a failed switch to Everything; live WM behavior for Calculator remains untested.
- **Q2.** When Sugarglider parks the focused window, does macOS send an activation or main-window change, and does it arrive with `Quiet::No`? When no member can take focus, does activating Finder (R12, step 6) take key focus away from the parked window? Does Finder report a main-window change after that activation, for example to a parked Finder window?
  - Not blocking. R25's wait covers the switch's own raise and its parking echoes, so it ignores an activation that the switch caused whether or not macOS sends one. If such an activation can arrive after the wait ends, R25's end conditions change. Step 6 of R12 runs whether or not the parked window keeps key focus. If Finder reports a main-window change after its activation, R25's wait for step 6 changes.
- **Q3.** When the user launches an app, does `ApplicationActivated` or `ApplicationMainWindowChanged` reach the reactor before the new window does (`ApplicationLaunched`, `WindowsDiscovered`, or `WindowCreated`)? Sugarglider keeps each app's events in order (R24), so this is only about the order of the Accessibility notifications.
  - Not blocking. R38 decides membership when the reactor first sees the window, and R24 waits for a window the reactor hasn't seen. Either order gives the same result.
- **Q4.** When the user quits an app with ⌘Q, do `WindowDestroyed` events reach the reactor before `ApplicationTerminated`? How long is the gap? Check several apps, including Chrome and a single-window app. For an app with several windows, can a window-server update that lists its remaining windows arrive between the first `WindowDestroyed` and `ApplicationTerminated`? Before the M5b decision, such an update deleted the first window's records during the quit.
  - Not blocking. R23 keeps a closed window's records through either order of `WindowDestroyed` and `ApplicationTerminated`, and it no longer takes a window-server update as a signal that an app still runs, so an update inside the quit gap changes nothing.
- **Q5.** How long does a full switch take with about 20 windows? Log the duration of every switch, as Rooms does, and set a target from the measurement.
  - Not blocking. Nothing depends on the number.

## Milestones

Each milestone is a set of small commits that build and pass `cargo test`. Run `cargo +nightly fmt` before each commit. The feature stays behind `settings.experimental.contexts.enable` until the last milestone. Commits use `internal:` (or `refactor:`/`test:`) until the feature leaves experimental; then a `feat:` commit adds the release note.

M2 through M10 are integrated in this working branch. A person still needs to run the M1 physical spike and manual QA.

### M1. Spike: answer the macOS questions

- Add `devtool` subcommands: `park <pid> <window>`, which prints the frame it replaced, and `set-frame <pid> <window> <x> <y> <w> <h>`, which puts the window back. Reuse `devtool list ax` to read results.
- For Q1, a person runs them with Sugarglider stopped, so it doesn't lay the window out again.
- For Q2 to Q4, a person records a trace with `--record` while parking the focused window, launching an app, and quitting apps with ⌘Q. The trace shows the event order.
- The person uses a Mac with two displays and writes the answers to Q1–Q4 into this spec. This happens at the end, after the other milestones.
- Commit: `internal: add devtool commands to park windows`.

### M2. Model

- `src/model/contexts.rs` with the types, `rank`, `match_windows`, and `plan_switch`, plus unit tests for both scopes.
- No behavior change.

### M3. Parking and the journal

- The park and unpark primitives, with the parked set (H2), the frame bookkeeping (H4), and the tab checks that skip parked windows (H5).
- Each display's full bounds next to its visible frame, in `ScreenInfo` and the reactor's `Screen`, for H1's bottom-corner selection and readback check.
- The journal with R30 and R31, including the removal of entries on `WindowDestroyed` and `ApplicationThreadTerminated`, and injectable file paths.
- R34, restoring the journal per app at launch.
- Extend the test harness (see Implementation traps).
- Nothing calls these paths yet, apart from launch recovery.

### M4. Context layouts

- L1 to L3, L7, and L9 in `LayoutManager`. This covers `context_layouts`, the `active_mapping` and `active_mapping_mut` accessors, the `ActiveContext` passed in with `SpaceExposed`, the fallback in `layout(space)`, `NextLayout` and `PrevLayout` doing nothing under a context, `SpaceLayoutMapping::from_layout`, deleting a context's layouts, and dropping unknown ids at load.
- M4 adds every field that `LayoutManager` serializes, which is `context_layouts` with `#[serde(default)]`, and blesses the snapshot. Later milestones add no serialized field. `Contexts` never goes into `layout.ron`. The reactor owns it and loads it from `contexts.json`, and any serialized struct that holds it marks it `#[serde(skip)]`.
- Model tests assert exact frames.

### M5a. Switching in global scope

- R7, R10, R12 (steps 1 to 5), R13 to R19, R27 to R29, R32 with the pending exit, and R33 with `SpaceManager`'s event.
- H3, L4's member filter in `send_visible_windows_to_layout`, L5, L6, L8, and L10. Floating members return at their journal frames (H3).
- Applying the active context inside `SpaceChanged` and `ScreenParametersChanged` (L2), once at `StartupComplete`, and after a config reload that turns contexts on; the last two run the switch's focus step when the apply parked the focused window (R12). Each app joins its context as it registers (R38).
- Parked windows are parked again when a display change moves or resizes their screen, and at every switch (`repark_moved_windows`), with a fresh journal frame first when the old frame is on no screen.
- The switching commands (`switch_context`, `show_everything`, and `previous_context`) with the tagged `ContextRef`.
- `contexts.json` and the `enable` config flag.
- The stored boot id, and the call to `Contexts::forget_window_server_ids` at a launch after a reboot (R22).
- Reactor tests that run H4 and H5 through real switches.
- Log the duration of every switch and answer Q5.

### M5b. Membership and focus

- R3, R20 to R25, R36 to R40, and step 6 of R12.
- Title tracking (R22), with an app event from the `kAXTitleChangedNotification` branch, a reactor event, and the record update.
- H2's single predicate on every path that sends a layout event, and the reactor's set of seen windows (R38).
- `plan_switch` takes "not in the visible-window set" in place of the `minimized`, `app_hidden`, and `unseen_space` flags that M2 gave `SwitchWindow` (R14).
- L11.
- The membership commands `add_window_to_context`, `move_window_to_context`, `remove_window_from_context`, and `toggle_window_pinned`.

### M5c. IPC and a minimal command line

- I1 to I4.
- The `sugarglider context` subcommands `list`, `create`, `add`, `switch`, and `everything`. They are enough to use contexts every day and to test them by hand.

### M6. Full command line and Raycast

- The other `sugarglider context` subcommands and `contrib/raycast/switch-context.sh`.
- `context forget <query> <record>` removes a gone window's member record from a named context. The record index comes from `context list --json` (`Contexts::remove_record`, R23).

### M7. Menu bar

- The status title and the context menu section.

### M8. Switcher

- The SwiftUI panel and the Swift bridge functions.

### M9. Per-screen scope

- R8, R9, R11, and R26, the `scope` config key, and `per_screen` inputs to `plan_switch`.
- R8 moves windows to another display, and so to another Space. Layouts are per Space. A window that R8 or R9 moved keeps its saved tile position in the layout of the screen it left; that screen closes the visible gap while the window is away, and the window returns to the saved position. Two-screen model and reactor tests cover the move and return.
- Model and reactor tests with two screens.

### M10. Preferences and docs

- The Preferences scope picker and its Rust config round-trip (the experimental contexts switch was already there).
- A user page in `site/src/content/docs`.
- Rust and Swift tests cover the scope picker and Preferences save path. Live Preferences use remains part of manual QA.

## Testing

Model tests (M2):

- A switch parks exactly the windows that must not show. An app with windows inside and outside the target context gets only its non-member windows parked (R12, R15).
- A switch doesn't park untracked windows, Sugarglider's own windows, or windows outside the visible-window set (R14). M5b updates these tests when it replaces the `minimized`, `app_hidden`, and `unseen_space` flags.
- Matching follows the R22 order. Step 1 needs the same app, and steps 2 and 3 never match a blank title. Steps 3 and 4 never take another context's window. Step 4 fills only the switch target's records, with windows that are on screen, and it never runs when a window arrives. Windows matched together get the same records in any order. Forgotten window server ids match nothing.
- A record follows its window's title. A relaunched window with the last title rejoins at step 2 (R22).
- Pending records match nothing. They stay when the app terminates, and they go when the app shows it is still running (R23).
- A single-window app that stays running after ⌘W keeps its record pending. The record stays when the app quits (R23).
- A quitting app leaves at most 50 empty records in each list that held its records, and drops the oldest first. Loading keeps every record. `remove_record` removes a record with or without a window (R23).
- Deleting the active context counts as a use of Unsorted and never leaves the previous context equal to the active one (R6, R18, R19).
- Ranking: "cli" finds "Client work", "cw" finds it by initials, and an exact name beats a prefix.
- Giving a context a number that another context holds takes the number from that context (R5).
- A pinned window shows under Unsorted and doesn't count as unsorted (R3, R29).
- `contexts.json` loads `active` as a context id, `"everything"`, or `"unsorted"`. A missing or unknown value loads as Everything.
- M9: a window shared by two contexts, in `per_screen` scope, goes to the screen that switched last, and keeps its saved place in the layout of the screen it left (R9).

Parking and journal tests (M3):

- The production selector uses full display bounds, chooses only a clear bottom corner, and rejects a readback beyond the 1 by 32 point bound or on another display (H1). Repark preflights the whole batch before changing any journal entry.
- Park and unpark writes take a new transaction id, update `frame_monotonic`, and clear `frame_attempts` (H4).
- Two parked windows of one app with the same size. One of them closes, and its nodes are removed (H5).
- A failed journal write parks nothing (R30).
- A journal entry goes on the `Requested(true)` echo within 16 points of the target, on `WindowDestroyed`, and on `ApplicationThreadTerminated` (R31).
- A launch with a journal restores each app's windows when that app arrives, before any context applies to it, and drops the entries of absent apps at `StartupComplete` (R34).
- An unreadable journal is moved aside and a new one starts (R34).

Layout tests (M4):

- While a context is active, a resize or a split changes the context's layout and leaves Everything's layout alone. `NextLayout` and `PrevLayout` do nothing (L1, L2).
- Turning off scroll layouts converts context layouts too (L1).
- A `SpaceExposed` with a new screen size while a context is active gives that context a separate layout for the new size. The old size brings the old layout back (L2).
- A missing context mapping makes `layout(space)` fall back to Everything's layout and log an error (L2).
- Deleting a context removes all its layouts. Loading drops `context_layouts` entries with unknown ids (L2).
- A context's first layout on a Space keeps the members' arrangement (L3).
- Changing C's layout kind, or floating and unfloating a window in C, leaves the window's node in D's layout (L9).
- Floating a window removes it from the layouts of every screen size of the context the Space shows (L9).

Reactor integration tests (M5a to M5c), using `Apps`, `simulate_until_quiet`, and `layout.calculate_layout`. Focus tests capture `raise_manager_tx` (see Implementation traps).

M5a:

- Switching away from a context and back gives exactly the same frames. This is the regression test for the layout constraint above.
- A switch parks every non-member window on the visible Spaces and nothing else (R13, R14).
- A member with no node in the target layout is unparked and tiled when its context becomes active (L8).
- A floating member returns at its journal frame (H3).
- Toggling between two contexts 10 times quickly ends with the right frames (H4).
- Two parked windows of one app with the same size. One of them closes during a switch cycle, and no empty tile stays in its context's layout (H5).
- A window that moves to another screen while C is active doesn't go back to the old screen when D becomes active (L10, R7).
- A failed journal write stops the switch, and the old context stays active (R12, R30).
- A `SpaceChanged` to a Space whose context has no mapping yet creates the mapping and applies the context in the same event (R10, L2).
- Showing Everything restores all windows (R27).
- `save_and_exit` puts every parked window back, keeps the saved active context, and exits after the last confirmation. When the deadline passes first, the journal stays on disk (R32). Make the exit call injectable for this test.
- Turning Sugarglider off, or turning off one space, shows Everything first. A `SpaceChanged` with `None` from the login window changes nothing (R33).
- Each `ContextRef` form survives a RON round trip. A bare integer is a number, and `id(7)`, or the older spelling `Id(7)`, is an id. In TOML, `{ id = 7 }` is an id.

M5b:

- After a restart with a named context active, a window that matches no record stays unsorted and is parked. It doesn't join the active context (R38).
- A new window joins the active context (R20). A window that arrives through `WindowCreated`, `WindowsOnScreenUpdated`, and `WindowBecameVisible` gets one node (R38, L11).
- A closed window's records go once its app shows it is still running (R23).
- An app that quits and relaunches while another context is active rejoins its contexts by title. Switching back unparks and tiles its window (R21, R22, L8). This also works when `WindowDestroyed` arrives before `ApplicationTerminated` (R23).
- A title change updates the window's member records (R22).
- A known non-member that is unminimized, or moves in from another Space, without taking focus is parked (R39).
- Focus on a window from another context switches and focuses that window (R24, R12 step 5). An activation before the switch's raise sequence ends does not switch (R25). Focus on an untracked window does not. Launching an app never switches to Unsorted, whatever order the launch events arrive in (R24).
- ⌘-Tab to an app whose main window is parked, and that has a visible member of the active context, raises that member and doesn't switch (R40).
- A switch that leaves no window to focus activates Finder and doesn't switch again, even when Finder's main window is parked (R12 step 6, R25).
- A new tab joins the contexts of its group's main tab (R36).
- `add_window_to_context` under Unsorted keeps the window visible and tiled until the next switch command; a Space change doesn't end that grace. `move_window_to_context` parks the window and focuses the active context's most recently focused member (R37, L2).

M5c:

- `sugarglider context create X` followed at once by `sugarglider context switch X` switches to X (I3).
- `create` makes a context of the tracked windows on the visible Spaces and makes it active.

Manual QA (a person, on a real Mac):

- The spike (M1) for Q1 to Q4.
- Repeat the WindowServer `OnScreenOnly` check with other apps and two displays. Check whether Mission Control shows a parked window and whether it stays accessible after a server crash without relaunch (L5, R35).
- Two displays in both scopes.
- WhatsApp in two contexts. Chrome with one window in each of two contexts, before and after quitting and relaunching Chrome.
- Finder windows in and out of a context.
- ⌘-Tab, a Dock click, and a notification click into another context. Opening a Raycast or 1Password panel from inside a context.
- A switch to an empty context while typing in a terminal. Keystrokes don't reach the parked terminal (R12, step 6).
- A display above another display; verify the upper display falls back to Everything if neither bottom corner is clear (H1).
- Quit from the menu with windows parked. Every window is back before the process exits (R32).
- Lock the screen while a context is active. Nothing comes back from parking (R33).
- `kill -9` on the server while windows are parked, then relaunch. Also check R35 without relaunching.
- `save_and_exit` and `--restore`.
- The Raycast script. SuperCmd importing the script folder.
- ⌃⌥Space with two keyboard input sources enabled.

## Later

- Open the apps of a context that aren't running, and match their windows as they appear.
- A Raycast extension with a searchable list.
- Layout previews in the switcher.
- Keeping layout shapes across reboots.
- A `SIGTERM` handler that shows Everything before exit.
- A `sugarglider context recover` command that restores the journal without the server.
- Replay of a session that uses contexts. A recording now carries the contexts read at launch and at a reload, the journal, the layout after the contexts pruned it, and the process checks the launch recovery makes; a replay answers from the recording. The 2-second deadline ticks of the switch guard and of the quit depend on the wall clock and aren't recorded, so a replay still doesn't reproduce them.
- A URL scheme (`sugarglider://context/<name>`). It needs app bundle work first: `glide_bundle()` requires a bundle id containing "glidewm", and the packaged id is `com.rdbeerman.sugarglider` (`src/sys/bundle.rs`).
- Per-context floating frames.

## References

- Rooms, MIT license: [README](https://github.com/saragordic/rooms). Relevant files: `Sources/Rooms/Windows/WindowEngine.swift` (parking and the resting ledger), `Sources/RoomsCore/Geometry.swift` (`parkingOrigin`, `keptOnScreen`), `Sources/RoomsCore/WindowSlot.swift` (`SlotMatcher`, `RestLedger`), `Sources/RoomsCore/Matcher.swift` (name ranking), `Sources/Rooms/Palette/PalettePanel.swift` (the panel).
- [AeroSpace](https://github.com/nikitabobko/AeroSpace) hides windows by moving them to a screen corner.
- [Raycast script commands](https://github.com/raycast/script-commands).
- [SuperCmd extension support](https://supercmd.sh/).

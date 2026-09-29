# Contexts QA attempt, September 29, 2026

Physical QA remains incomplete. The first attempt stalled in native automation. A second human-launched run completed one-display probes and found a native-tab detection defect. Only one display was available. The [Contexts specification](contexts.md) remains canonical, including its settled decisions and manual checklist. PR #25 remains draft.

## Source and local proof

The tested source was `784d02c2ac469be11378b7a1273b7d4149f50ecb` on `ctx-final-sol`. The integration worktree was clean, and the fork branch `feat-rooms-context-switching` and PR #25 matched that head. The existing integration agent was idle before preparation.

The fresh command `cargo build --locked --offline --bins --example devtool` passed. The server, CLI, and devtool binaries were then hashed for the launch check. The temporary global-scope config passed `sugarglider config --config <path> verify`.

The prior integration report records 234 focused context tests; 994 Rust library, 34 CLI, 6 example, and 7 doctests; 190 Swift tests; Rust and Swift builds; nightly formatting; strict prose lint; and a seven-page site build. This attempt did not repeat those suites. Terra's prior scoped review found no remaining false-success path. The prior Claude CLI review did not run because of DNS failure.

## Hosted proof

The [Rust build and test run](https://github.com/rdbeerman/Sugarglider/actions/runs/36485815836) passed for head `784d02c2ac469be11378b7a1273b7d4149f50ecb`. Its formatting, build, and test steps passed; the job completed on September 28 at 21:26:17 UTC. This is hosted source validation, not physical macOS proof.

## First attempt

Tim launched the built server at 00:57:32 local time on September 29 with `--one`, a temporary config enabling contexts, and a reactor recording. The server ran as PID 37399. Agents did not launch it.

- The first CLI ping succeeded. The context snapshot reported global scope, Everything active, one screen, and no contexts.
- Devtool reported one 1512 by 982 point display and test Space 638. Its usable frame was `(0, 33, 1512, 949)`.
- The baseline AX listing on that Space contained one standard Ghostty window, WindowServer ID 71490, at the usable frame. This was a post-launch baseline. It does not establish the terminal's frame before the manager started.
- CUA explicitly denied access to Terminal. A Finder observation took about 16 minutes before launch. After launch, a TextEdit observation took about 9 hours and 37 minutes despite a requested 20-second timeout. These were tool stalls, not measured Sugarglider operation times.
- No agent click, keystroke, test document creation, context creation, or parking probe completed. The TextEdit observation returned an existing Open dialog.
- At 08:35:58 UTC, the server was still running. The desktop showed Space 1. The snapshot still reported Everything and no contexts, with no managed screen because `--one` restricted management to Space 638.
- A subsequent `context everything` command reported `No Space is managed right now`. It was not a successful restoration test. `sugarglider pause` succeeded. The next snapshot still reported Everything, no contexts, and no managed screen.

The recording includes incidental app and Space events during the stalled interval. It is not a controlled launch, quit, focus, or lock test and does not answer Q2 to Q4.

## Recovery and local artifacts

Before launch, the layout and normal config had checksum-matched backups. There were no `contexts.json` or `parked.json` files. After the pause, both files were still absent, and the original layout and config still matched their backups. No context window was parked by this attempt. No disposable test window was created.

The server was paused at the last recovery check. Clean exit still requires verification. The normal exit routes are Alt+Shift+E and Quit in Sugarglider's menu. The CLI has no clean-quit command, and CUA was not reliable enough to operate the menu. Do not use Ctrl+C or SIGTERM as a substitute for the clean-exit test.

Local artifacts are under `/private/tmp/sugarglider-qa-20260929` on the laptop:

- `backup-receipt.json`, the original layout and config copies, and `binaries.sha256`;
- the verified `config.toml`, guarded `launch.sh`, build log, and baseline AX and WindowServer listings;
- `run-20260929-005732-37390/trace.ron` and `server.log`;
- `qa-plan.md`, the procedure for resuming the checklist.

These temporary files are not repository artifacts and may disappear after cleanup or reboot. The trace contains window titles and remains local. Preserve any nonempty parking journal until every affected window is recovered.

## Second run and stopped-manager probes

Tim launched PID 21103 at 21:19:15 UTC on September 29 on dedicated Space 638. The source head was `99b6ddea590a2e4557694f2c04c3d6c010efc2aa`, whose only change from the tested implementation was documentation. The server used the validated temporary config. The display was 1512 by 982 points, with usable frame `(0, 33, 1512, 949)`.

The local evidence directory is `/private/tmp/sugarglider-qa-20260929`. It contains `run-20260929-231915-21089/trace.ron`, the server log, named CUA observations, and the failed timing sample. The separate reactor log is `/private/tmp/glide.21103.log`. These local artifacts contain window titles and are not committed. This report records the observations needed for review.

| Probe | Observed result | Limit |
| --- | --- | --- |
| TextEdit contexts | Created QA-A and QA-B, switched between them, used Previous and Everything, and restored their windows. A parked TextEdit window had AX frame `(1511, 950, 302, 949)`, a 1 by 32 point strip. WindowServer OnScreenOnly listed it. | One display and these windows only. |
| Focused parked TextEdit | Explicit activation of parked QA-B switched to Unsorted and restored and focused QA-B. | Finder key focus, notification entry, and typing into a parked terminal remain unverified. |
| Switcher | The 600 by 460 point panel appeared at WindowServer layer 3. Return selected QA-B using the window focused before the panel opened. | Create, edit, filter, Preferences, and launcher flows remain open. |
| Calculator | PID 70346, window 74826, refused parking and stayed at `(758, 197, 230, 408)`. The live manager fell back to Everything after its two-second deadline. | This verifies refusal handling, not native recovery from a parked Calculator window. |
| Chrome | PID 79992's window returned `(1511, 941, 500, 949)`, a 1 by 41 point strip. The manager rejected it and restored Everything. Both test windows returned. | H1 remains 32 points. Tim authorized investigation of a revised bound; no replacement bound is approved. |
| Initial helper-window refusal | Two automation-helper windows entered the first parking batch. Missing confirmation caused the live manager to restore Everything. They were pinned for subsequent probes. | A helper journal entry remains as evidence of missing restoration confirmation. It does not represent a window still visibly parked. |

The launch and quit trace contains these event times, all UTC on September 29:

| App | Launch observation | First WindowDestroyed | ApplicationTerminated | Gap |
| --- | --- | --- | --- | --- |
| Calculator, PID 70346 | ApplicationLaunched at 21:32:26.389030, `is_frontmost: false` | 21:33:00.546111 | 21:33:00.597263 | 51.152 ms |
| Chrome, PID 79992 | ApplicationLaunched at 21:33:29.025572, `is_frontmost: true` | 21:36:05.019216 | 21:36:05.264132 | 244.916 ms |
| TextEdit with 21 native tabs, PID 97211 | The group was assembled after launch. | 21:43:46.356179 | 21:43:46.357170 | 0.991 ms |

For Q3, the reactor first received a WindowServer update for Calculator at 21:32:26.387417, then ApplicationLaunched with window 74826 already present at 21:32:26.389030. Chrome followed the same order at 21:33:29.025285 and 21:33:29.025572, with window 74846 included. These observations do not prove the notification order for other launch paths. `run2-lifecycle-order.json` records the bounded trace extraction.

Chrome's second window was destroyed at 21:36:05.020191. For TextEdit, the remaining 20 WindowDestroyed events arrived after ApplicationTerminated, from 21:43:46.358150 through 21:43:46.362343. These traces demonstrate several event orders. Chrome was quit through its native menu after its hold-to-quit prompt prevented the synthetic shortcut. They do not close Q4's literal Cmd+Q, relaunch, and record-survival matrix.

### Native tabs and timing failure

The 21-window TextEdit setup exposed a defect in frame-based tab detection. Minimum window widths caused independent windows to overlap at the right edge. Creating the intended ten-window context produced 19 members. That invalidates the timing sample: the CLI returned after 82.621 ms, but the expected window set had not settled after 4.981 seconds. `perf-results.json` records one failed sample. No successful median, p95, maximum, or Q5 target can be derived from it.

After merging the windows into an actual native tab group, AXWindows and WindowServer OnScreenOnly exposed only its active window. The native tab bar exposed 21 buttons, but every button's AXWindow identified the active window. It did not identify the inactive windows. A later stopped-manager probe retained three window references, merged them, and selected another tab. The bar and button identities stayed equal while the selected window ID changed. `cached-tabs.jsonl` and `tab-links.jsonl` record these probes.

Tim chose explicit identity tracking. Each tab becomes identified when the user selects it. Contexts must show Everything with an explanation until every tab is identified; it must not cycle through tabs automatically. The local implementation replaces frame grouping with that rule and adds `devtool native-tabs <pid>` for read-only observations. Local model and reactor tests cover membership, overlapping independent windows, close ordering, detachment, and layout preservation. The compiled reader reported the observed Dia and Chrome windows as standalone, without selecting or moving them. Those checks are in `native-reader-dia.txt` and `native-reader-chrome.txt`. The compiled-reader follow-up below verifies actual native groups. Physical verification of the replacement live manager remains open.

### Exit and restoration

Neither the synthetic nor human clean-exit shortcut produced a SaveAndExit event in this run. Tim stopped the server with Control+C after Everything had been restored. This is not an R32 clean-exit pass. No kill test was performed.

The session closed its disposable TextEdit, Calculator, and Chrome instances. It preserved Tim's original TextEdit process 33920 and restored window 74723 to `(152, 73, 586, 488)`. It restored Ghostty window 74072 to `(0, 33, 1194, 949)`. CUA independently confirmed both frame writes. Shared automation helpers were left running. `parked.json` retains the helper entry for PID 8030, window 157, with original frame `(0, 0, 1512, 982)`; AX and WindowServer showed that frame, but the manager had not confirmed it. The journal, trace, and checksum-matched pre-QA backups remain available.

A later isolated Chrome probe opened on Space 1. It was closed without a parking write because the controlled test Space was not current. The production parking bound was not changed.

Desktop icons became visible during the session although `StandardHideDesktopIcons` still read `1`. No test changed that preference. A desktop-reveal or focus effect is possible, but its trigger is unconfirmed.

### September 30 native reader follow-up

These stopped-manager probes ran on macOS 26.6.2, build 25G83, arm64. A disposable AppKit app exposed three native window tabs and a separate `NSTabView` in one ordinary window on Space 638. The first reader incorrectly grouped that ordinary tab view. AXContents distinguished the controls: the native bar contained its tab buttons, while NSTabView contained the selected page. The revised reader requires the native contents relationship and rejects contradictory button-to-window bindings.

Dragging native tab A from first to last preserved CFEqual identities for the bar and each button. Selecting C changed the exposed window from 76219 to 76221 while retaining those identities. The rebuilt reader then learned A, C, and B as one complete group and reported window 76218 as standalone with its ordinary tabs selected. The 240-sample run completed with exit 0. A tooltip window returned an AX error, which the diagnostic reported separately instead of treating it as a standalone window.

A fresh TextEdit instance, PID 2711, provided the real-app cross-check. Its two disposable documents were merged through Window > Merge All Windows. The reader first reported window 76253 and one unknown member, then reported both 76253 and 76252 after selecting B. The 30-sample run completed with exit 0. The app fixture and TextEdit instance were quit through their menus, and fresh inventories confirmed neither retained a window. The prior Space 1 and Orca window were restored without changing their frames. Tim's existing TextEdit and Ghostty instances were preserved.

The evidence is in `tab-fixture/identity.jsonl`, `reader-fixed-2.txt`, `textedit-reader.txt`, the fixture source, and screenshots under the same local QA directory. These probes establish reader behavior on this macOS installation. They do not establish live manager switching, parking, or recovery behavior.

A final fixture added an empty NSTabView. The reader reported both ordinary tab-view windows as standalone and retained the three-member native window group. Missing native attributes on a new control now mean it is not a native bar; transient errors and missing attributes on a known bar still fail closed. `tab-fixture-v2/reader.txt` and its screenshots record that probe. The fixture was closed and the previous Space restored.

### September 30 Chrome bound investigation

Chrome 154.0.8037.58 ran in a new temporary profile as PID 35691 with one blank window, 76429, on Space 638. With Sugarglider stopped, its original frame was `(22, 55, 1200, 905)`. The production parking target was `(1511, 981, 1200, 905)`. All three AX attempts returned `(1511, 941, 1200, 905)`, leaving a 1 by 41 point strip. The 32-point rule rejected it. WindowServer still reported the window on the current Space.

The native Window > Zoom command restored it to `(0, 33, 1512, 949)`. A screenshot and AX controls showed the usable browser window. The devtool then restored exactly `(22, 55, 1200, 905)` on its first attempt. Chrome was quit through its native menu; a fresh inventory showed no windows for that test PID. The previous Space and app were restored. The original Chrome process was untouched.

Evidence is in `chrome-bound-park.txt`, `chrome-bound-parked-windows.json`, `chrome-bound-zoom-settled-windows.json`, the before/after screenshots, and `chrome-bound-restored.txt`. This establishes a native recovery route for this one window, without a running manager. It does not establish Mission Control, crash recovery, other Chrome versions, or two-display behavior. A 41-point production limit has been proposed to Tim; approval is pending, and H1 remains 32 points.

### Source checks after the tab fix

The latest full `cargo test --locked --offline` run passed 1,024 library tests, 34 CLI tests, six devtool tests, and seven doctests. The final review corrections then passed all 33 focused native-tab tests and the binary build. These cover a refused write to a known inactive tab and refusals outside managed context windows. The inactive-tab regression failed before the correction and passed afterward. The server, CLI, and devtool build, nightly formatting, strict prose lint, and seven-page site build passed. Swift source did not change; its prior 190-test result was not rerun.

The first independent Claude review ran and found defects in closed-tab tracking, parked-tab recovery, and ordinary tab-view classification. Follow-up changes remove closed and moved slots, retain recovery entries for all identified members, restore through the selected tab, validate AX ownership before writes, and distinguish NSTabView controls. They also centralize inactive-tab exclusion, enforce Everything on passive events, preserve policy while discovery is incomplete, and avoid restoring expired add grace. The first re-review confirmed the main tab-close and recovery fixes. Its remaining error-classification findings led to a separate frame-target refusal event, known-bar-aware AX qualification, and clearing old member read failures after a valid group observation. The frame-target refusal still restores Everything on AX/WindowServer disagreement; it does not invent a native-tab group. The final scoped review completed and confirmed those corrections. It identified two remaining cases: missing AXChildren on an ordinary window and a late frame refusal after a visible native tab selection. The final changes apply the existing unknown-control qualification to AXChildren and ignore a refusal for a known inactive, unparked tab. The 256-child discovery cap still fails closed because an unread bar could lie beyond it. A final read-only Claude check of these two narrow follow-ups found no actionable defects. It ran no additional tests; the focused checks above cover the final code.

The reviewer also flagged a possible Everything fallback when a destroyed window disappears from AX before WindowServer. The agreed immediate fallback remains, including a brief false fallback during Cmd+W. A tab-selection event that arrives after a frame refusal can likewise cause a conservative fallback. Their frequency, default-off AX cost, and frame-write validation cost remain physical QA questions. Selection before restoration confirmation can still restore Everything, and unvisited members retain their journal entries until identification or app exit.

The native-tab correction is commit `d4f7bc0b066779059b01af7fd71600a991d395c9`. It has not yet received physical live-manager proof. At the September 30 pre-push check, the remote head remained `99b6dde`, with its [Rust CI run](https://github.com/rdbeerman/Sugarglider/actions/runs/36544259926) passing. That hosted result predates the local tab fix.

## Remaining proof

| Check | Evidence still required |
| --- | --- |
| Q1, L5, and R35 | Other apps, both displays, accepted parking bounds, OnScreenOnly membership, Mission Control, and native recovery without relaunch. The TextEdit, Calculator, and Chrome observations remain limited to those windows. |
| Q2 | A controlled focused-window parking trace, Finder activation and main-window ordering, Quiet values, and proof that typing cannot reach the parked terminal. |
| Q3 | A controlled app launch with activation, discovery, and creation ordering, followed by membership verification. |
| Q4 | Controlled Cmd+Q and relaunch for multiwindow Chrome and a single-window app, timestamped event gaps, and membership survival through intervening WindowServer updates. |
| Q5 | Completed switches with about 20 real windows, sample counts, failure counts, median, p95, maximum, and a target based on those measurements. The existing `Switched context` elapsed field ends after requests are queued; it does not measure the final AX frame confirmation. |
| Both scopes | Global and per-screen switching, shared-window move and return, saved tile positions, distinct Spaces, blocked bottom corners, and stopped scope-change startup selection. One display cannot establish these two-display behaviors. |
| UI and membership | Create, switch, Everything, restoration, add grace, move, remove, pin, switcher target identity, menu actions, and Preferences persistence. |
| Focus and panels | Cmd+Tab, Dock and notification entry, shared WhatsApp and Chrome windows, Finder in and out of contexts, and launcher or 1Password panels. |
| Lifecycle | Menu quit with parked windows, save-and-exit plus restore, lock and unlock, and coordinated crash recovery with and without relaunch. |
| Launchers and input | The shipped Raycast script, SuperCmd import and use, and the switcher hotkey with two input sources. The script expects `/usr/local/bin/sugarglider`, which was absent on this laptop. |

Resume on the dedicated Space with reliable native control or direct human operation. A second physical display is required for the display cases. Start with reversible tests and retain the original frames and journal before coordinating a kill scenario. Neither local tests nor devtool alone can close these live-manager checks.

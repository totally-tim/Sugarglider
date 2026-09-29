# Contexts QA attempt, September 29, 2026

Physical QA remains incomplete. Native automation stalled before any context or parking test, and only one display was available. No product defect was established by this attempt. The [Contexts specification](contexts.md) remains canonical, including its settled decisions and manual checklist. PR #25 remains draft.

## Source and local proof

The tested source was `784d02c2ac469be11378b7a1273b7d4149f50ecb` on `ctx-final-sol`. The integration worktree was clean, and the fork branch `feat-rooms-context-switching` and PR #25 matched that head. The existing integration agent was idle before preparation.

The fresh command `cargo build --locked --offline --bins --example devtool` passed. The server, CLI, and devtool binaries were then hashed for the launch check. The temporary global-scope config passed `sugarglider config --config <path> verify`.

The prior integration report records 234 focused context tests; 994 Rust library, 34 CLI, 6 example, and 7 doctests; 190 Swift tests; Rust and Swift builds; nightly formatting; strict prose lint; and a seven-page site build. This attempt did not repeat those suites. Terra's prior scoped review found no remaining false-success path. The prior Claude CLI review did not run because of DNS failure.

## Hosted proof

The [Rust build and test run](https://github.com/rdbeerman/Sugarglider/actions/runs/36485815836) passed for head `784d02c2ac469be11378b7a1273b7d4149f50ecb`. Its formatting, build, and test steps passed; the job completed on September 28 at 21:26:17 UTC. This is hosted source validation, not physical macOS proof.

## Physical observations

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

## Remaining proof

| Check | Evidence still required |
| --- | --- |
| Q1, L5, and R35 | Other apps, both displays, accepted parking bounds, OnScreenOnly membership, Mission Control, and native recovery without relaunch. The earlier TextEdit and Calculator observations remain limited to those windows. |
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

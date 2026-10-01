# Context launcher verification, October 2, 2026

The requested workflow is direct search: open Raycast or SuperCmd, type a context name in the main search, and press Return on that context. Sugarglider generates one indexed command per context. Users import `~/.config/raycast/script-commands/sugarglider-contexts` once in Raycast's Script Commands settings. Installed SuperCmd 1.0.26 scans that directory automatically.

## Implementation

A worker receives published context snapshots without blocking the reactor. It keeps command filenames and actions tied to context IDs while updating display names. Creation, rename, deletion, Unsorted availability, and disabling Contexts trigger file updates. Scripts call the CLI beside the running server and preserve its errors. They never launch the manager.

The ownership manifest records writes before script replacement. The worker checks the whole batch, preserves user edits and unrelated files, rejects symbolic links, locks against another writer, and retries failed updates. Disabling Contexts removes owned commands unless a filesystem or ownership conflict blocks the batch. With the manager stopped, existing commands remain indexed and report that it is not running.

## Local checks

The initial full Rust run passed 1,036 library tests, 36 CLI tests, six devtool tests, seven doctests, and five helper-crate tests. Rust builds, nightly formatting, diff checks, strict prose lint, and the seven-page site build passed. Swift source did not change; its earlier test result is separate evidence.

The initial ten focused launcher tests cover lifecycle updates, stable-ID execution, numeric and hostile names, CLI failure status, metadata injection, missing binaries, unchanged-file timestamps, ownership conflicts, symlinks, interrupted writes, disabling, and competing writers. A follow-up focused run passed after bounding output size and completing pending manifests whose script writes had already finished. The review fixes then passed all 12 focused tests, including Unicode separators, control-only titles, and symlinked server paths. The final lock correction passed all 13 focused tests. The worker explicitly unlocks on every exit path, with a regression that retains a duplicate descriptor while another writer acquires the lock.

A separate temporary probe ran the actual background worker with an isolated data directory. It published 101 snapshots while holding the output lock, released the lock, verified that only the latest name appeared, and verified removal after disabling Contexts. This checks coalescing and retry behavior without a live manager; `worker-receipt.json` records the result.

The final complete check run passed 1,038 library tests, 36 CLI tests, six devtool tests, seven doctests, and five helper-crate tests after the review corrections. The final Rust build, formatting, prose lint, diff check, and seven-page site build passed as well. `final-checks.log` records that run. After changing the output folder and adding explicit lock release, the CLI path test, 13 launcher tests, and Rust build passed in `path-check.log` and `lock-checks.log`.

Artifacts live under `/private/tmp/sugarglider-launcher-qa-20261001`. The check logs above, `review-fixes-final-tests.log`, `worker-probe.rs`, and `worker-receipt.json` preserve the local results. The worker and fixture source hashes match the final launcher source.

## Installed SuperCmd code

The installed `/Applications/SuperCmd.app` reports version 1.0.26. Its bundled `dist/main/script-command-runner.js` was extracted read-only from `app.asar`. A temporary fixture generator compiled the current launcher module and generated named commands plus Everything and Unsorted. The final fixture also includes a Unicode line separator and a control-only name.

The installed parser discovered all five final titles, including the sanitized title and the nonempty fallback, read the Sugarglider Contexts package name, and required no arguments. Its runner invoked a generated command against the compiled CLI while no manager process existed. The result was exit status 1 with `Sugarglider isn't running.` This verifies the installed parser and execution path in isolation, with temporary settings and home-directory adapters. It does not prove visible search or successful window switching.

The initial receipt is `supercmd-parser-receipt.json`. A final run used an isolated home directory with no configured script folders. It automatically discovered the generated commands under `.config/raycast/script-commands/sugarglider-contexts` and returned the same stopped-manager error. `supercmd-auto-discovery-receipt.json` records this result. The installed parser skips dotfiles, including ownership metadata and temporary writes. `fixture-generator.rs`, `generated-fixture.json`, and `fixture-source.sha256` preserve the fixture and its source identity. The parser caches discovery for 12 seconds; root-search refresh timing still needs UI measurement.

## Native UI attempt and cleanup

Two checksum-matched fixture commands were temporarily placed in SuperCmd's default script directory, inside `sugarglider-qa-20261001`. The configured Command-Space shortcut sent through CUA did not open the launcher. Application activation returned `ambiguous_window_target` because the observed SuperCmd windows were hidden. No search text or command was sent to an unverified target.

The fixture files were compared against their originals, removed, and their empty directory removed. Launcher settings were not changed. No windows were parked, and the manager remained stopped. Screenshots and `ui-cleanup.json` record the attempt. The visible search and Return flow remains unverified.

## Independent review

The installed Claude CLI completed a read-only review in 582 seconds with no blocking defects and six lower-severity findings. Corrections stop idle polling, back off failures to 60 seconds, sanitize Unicode line separators, canonicalize the executable path, and qualify the documentation's ownership, ID-reset, and indexing claims. The scoped follow-up completed in 462 seconds. It found no defects in the corrected worker, title handling, or executable path logic. Its remaining lock-release issue was fixed with an explicit drop guard and focused regression. Documentation findings were corrected as well.

`claude-review.json` and `claude-followup.json` record both completed reviews. Backoff deadline suppression, growth to the cap, changed-catalog reset, and partial-batch reversion have source-review evidence; the runtime probe measures only the initial retry and coalescing. Abrupt process exit can leave temporary dotfiles; Raycast handling of those files remains unverified. An earlier review was accidentally submitted to a pane another session had occupied; that session stopped its duplicate attempt. Its aborted output is not review evidence.

## Remaining proof

- Open SuperCmd normally and verify automatic discovery of the generated folder. Search by context name, press Return, and verify the active context and window restoration with the live manager.
- Verify create, rename, and delete refresh in the open launcher, including stale search results, disabled Contexts, and a stopped manager. Check Everything, Unsorted, and both scopes.
- Repeat the flow in Raycast. Raycast was not installed on this laptop, so its behavior is supported by its [script-command documentation](https://manual.raycast.com/script-commands), not physical execution.
- Check other SuperCmd versions separately. The vendor now advertises a newer application; this run inspected the installed 1.0.26 implementation.

The broader [Contexts QA record](contexts-qa-2026-09-29.md) still governs parking, recovery, focus, lifecycle ordering, two displays, and switch timing. This launcher work does not close those questions.

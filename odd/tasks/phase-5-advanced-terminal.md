# Phase 5 — Advanced terminal

## Objective
Give so1term the modern-terminal features from planing.md §5 Fase 5: the kitty
keyboard protocol (terminal emits, so1sh consumes), the kitty graphics protocol
(inline images), a remote-control socket, and a persistent server with
detach/attach in the style of herdr.

## Problem / Why
Phases 3-4 delivered a block-based terminal with panes and tabs, but sessions
die with the window, keys are limited to legacy xterm encodings, images cannot
be shown inline, and nothing outside the window can drive it (agents in Phase 6
need that control channel).

## Scope
- In: kitty keyboard protocol (progressive enhancement flags, CSI u) in so1term
  and its consumption in so1sh; kitty graphics protocol (transmit/display/delete,
  at least PNG and RGBA, direct and file transmission); a local control socket
  with a small CLI; a persistent server process owning PTYs and models, with a
  client window that can detach and re-attach.
- Out: agent orchestration and permissions (Phase 6); network/remote hosts
  (local machine only); kitty "kittens".

## Constraints
- Platforms: Linux, macOS, Windows (sockets and process model must have a
  portable story; runtime checks on Linux).
- Licenses: kitty is GPL-3.0 — implement from its published protocol specs only,
  never copy code. WezTerm and Rio (MIT) and herdr (Apache-2.0) may be studied
  and reused with attribution.
- Security: the control socket must be private to the user (permissions,
  no network exposure); graphics file transmission must not read arbitrary
  files without the documented safeguards.
- Single writer: `codex-impl`. Orchestrator commits. Branch `feat/phase-5-advanced-terminal`.

## TDD
- Strict. Runner: `cargo test --workspace < /dev/null`, `make ci`.
- Protocol encoders/parsers are pure and table-tested; GUI behavior gets a
  Linux smoke with real typed input and screenshots.

## Delivery
- Strategy: `ask-on-risk`. Forecast: several thousand authored lines -> native
  review per work-unit slice as `review_due` requires.

## References
- kitty protocol specs: https://sw.kovidgoyal.net/kitty/keyboard-protocol/ and https://sw.kovidgoyal.net/kitty/graphics-protocol/ (spec only).
- `~/proyectos/so1Shell-refs/wezterm` (kitty keyboard + graphics, mux server/client, unix domain sockets).
- `~/proyectos/so1Shell-refs/herdr` (persistent server, CLI over socket).

## Decision (T0, Codex spike 2026-10-10; consistent with planing.md §4)
- Order: kitty keyboard -> kitty graphics -> control API -> persistent server (the server changes who owns every PTY/model, so it goes last).
- Keyboard: alacritty_terminal 0.26 already implements CSI u mode push/pop/query/set behind `Config::kitty_keyboard` (term/mod.rs:349-365,1275-1328) — our Model uses Config::default() (model.rs:346-360). reedline 0.52.1 has `use_kitty_keyboard_enhancement` (engine.rs:430-441); crossterm 0.29 parses CSI u on Unix only. Windows shell-side support not claimed until tested.
- Graphics: vte 0.15 has no APC hook (lib.rs:359,377,761-805) -> separate bounded incremental APC observer next to the OSC observer. Per-pane image store + placements anchored to stable rows (reuse the T2 eviction counter); alt-screen placements discarded on exit; width reflow invalidates placements. Textured quads clipped per pane. Limits: 128 MiB/pane, 512 MiB/process, dimension caps, bounded chunk state. File/temp transmission denied by default (opt-in allowed roots, canonicalized, no symlink escape); shared memory deferred.
- Control: versioned bounded JSON-lines API with request ids; Unix socket in a 0700 runtime dir (socket 0600, ownership check, stale-socket handling), Windows named pipe with current-user ACL, never TCP; worker thread parses, mutations go through EventLoopProxy; send-text and get-text treated as sensitive.
- Server: extract headless `crates/term-core` (model, blocks, layout) first; server owns PTYs + core state; GUI becomes a rendering/input client; initial snapshot + versioned per-pane deltas with resync; semantic input sent to the server; v1 = one attached GUI per session; attach resize policy decided in T11.

## Tasks
- [x] **T0 — Design spike** (codex-impl, read-only) — decision above.
- [x] **T1 — Terminal kitty keyboard mode** — `09668ed` (codex-impl, ~1150 changed lines mostly tables/tests): kitty_keyboard enabled; flags 1/2/4/8/16 encoded as CSI u; press/repeat/release from winit; legacy path unchanged when flags are 0; per-screen flag stacks tested. Mode query answered from the active TermMode in the Handler wrapper (alacritty reported the stack top, not the effective mode after CSI =; orchestrator checked the override does not delegate, so one reply). 140+22 tests 2/2 (orchestrator). Native review (review-ff194673e8cf5215, granted): BLOCKER R3-named-text-keys-dropped (Space dropped even without flags) corrected in `7e6bc44`, targeted validation approved, acknowledged. Gaps: Hyper/Meta bits unavailable from winit; Caps/NumLock initial state unknown; partial keypad mapping. Original: enable Term option, expose flags, CSI u encoding per flags, legacy unchanged; tests for push/pop/set/query, flag combos, alt screen.
- [x] **T2 — so1sh consumes the keyboard protocol** — `8475926` (codex, delegated): reedline kitty enhancement after bounded probe on Unix TTYs, SO1SH_KITTY_KEYBOARD=0 opt-out, popped before commands; T1 advisories folded (lone modifiers only with REPORT_ALL, keypad navigation not digits, no release for consumed shortcuts); PTY tests for Ctrl-C/D, Escape, modifiers, restore. Orchestrator checks: fmt ok; clippy -D warnings ok; so1sh 32+6+95 pass; so1-terminal 141+25+1 pass. Native review (review-0985f3c9d570b0e5, high, granted, 4 lenses): approved, acknowledged. Follow-ups (non-blocking): consumed-key set never cleared/stale entries (main.rs:66-82, 2057-2059); tautological kitty asserts and unexplained escape constants in repl tests (~2490-2500); verbose keypad table (main.rs:405-451).
- [x] **T3 — Incremental APC parser** — `9be6920` (codex, delegated): `crates/terminal/src/apc.rs` 7-bit APC observer (C1 not recognized, same as OSC observer), 4 MiB default cap, oversized discarded through ST, CAN/SUB and ESC-other abort, every-split test; wired per pane with a no-op `observe_apc_payload` hook for T4. Orchestrator checks: fmt ok; clippy -D warnings ok; so1-terminal 145+25+1 pass. Review assess (base 68bd32b, committed-only): medium, review_due=false, under_budget -> pending in slice.
- [ ] **T4 — Graphics data ingestion**: direct RGB/RGBA/PNG, ids, chunking, responses/quiet, quotas, off-thread decode, overflow/bomb tests.
- [ ] **T5 — Guarded file/temp transmission**: allowed roots, regular-file and size checks, no-follow, temp constraints.
- [ ] **T6 — Placement model**: per-pane store, stable-row anchors, delete semantics, eviction, alt screen, resize invalidation.
- [ ] **T7 — Textured image rendering**: GPU texture cache, clipped quads, z-order, Linux smoke.
- [ ] **T8 — Extract term-core**: headless model/blocks/layout crate; GUI unchanged behavior.
- [ ] **T9 — Local control API and CLI**: JSON-lines, private socket/named pipe, EventLoopProxy routing, auth and integration tests.
- [ ] **T10 — Persistent session server**: server owns PTYs/core state; startup, detach, shell exit, stop, recovery tests.
- [ ] **T11 — Attach protocol and GUI client**: snapshot/delta, resync, semantic input, image blobs, attach/detach smoke.
- [ ] **T12 — Phase verification**.

## Verification evidence
_(filled per task)_

## Progress / Next step
- Branch created from `main` (`60f2615`). T0-T1 done. Next: T2 so1sh keyboard consumption.

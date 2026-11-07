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
- [x] **T4 — Graphics data ingestion** — `feceb5c` + correction `1bebeea` (codex, delegated): `crates/terminal/src/graphics.rs` direct RGB/RGBA/PNG, zlib, m=1/m=0 chunks, ids, OK/error replies with q=1/2, 128 MiB/pane, 512 MiB/process, 10000px caps checked before allocation, oldest-image eviction, off-thread decode with per-pane in-flight guard. New deps: base64 0.22.1, flate2 1.1.5, png 0.18.0 (MIT/Apache-2.0). t=f/t/s and a=T/p/d reply ENOTSUP. Orchestrator checks: fmt ok; clippy -D warnings ok; so1-terminal 145+41+1 pass. Native review (review-39129c259a01276a, slice 68bd32b..1bebeea = T3+T4, escalated item 5 dependencies, granted, lenses risk/resilience/reliability): BLOCKER R3-transmit-and-display-reports-ok-without-display corrected in `1bebeea` (RED/GREEN test), targeted validation approved, acknowledged. Reviewed boundary -> 1bebeea. Follow-ups (non-blocking): decode thread panic leaves pane stuck decoding (main.rs:1938-1946); EBUSY drops back-to-back transfers (graphics.rs:220-229); base64 decode on UI thread (graphics.rs:314-318); transient/chunk buffers outside process cap (graphics.rs:14, 281-320); store linear scans/unaccounted overhead (graphics.rs:574-640); commands without ';' silently dropped (graphics.rs:198-201); main.rs integration path untested.
- [x] **T5 — Guarded file/temp transmission** — `6a88951` (codex, delegated): t=f/t=t denied unless SO1TERM_GRAPHICS_ALLOWED_ROOTS lists roots (canonicalized at startup); openat walk with O_NOFOLLOW, final open O_NONBLOCK, regular file + dev/inode match, S/O bounded by existing limits, reads off the UI thread; t=t needs the tty-graphics-protocol marker and is deleted after success; Windows denies file media; t=s ENOTSUP. Orchestrator checks: fmt ok; clippy -D warnings ok; so1-terminal 145+47+1 pass. Native review (review-d81d84a02734b520, escalated item 2, granted, lenses risk/resilience/reliability): approved without blockers, acknowledged. Reviewed boundary -> 6a88951. Known gap: temp unlink inode check and unlinkat are not atomic. Follow-ups (non-blocking): file errors collapsed to EPERM (graphics.rs:244-253); file-medium S conflated with zlib/PNG size (graphics.rs:745); temp unlink failure fails whole transfer (graphics.rs:527-529); stale root descriptor if a root is replaced (graphics.rs:496); some validation branches untested (graphics.rs:715-728).
- [x] **T6 — Placement model** — `6348513` (codex, delegated): `crates/terminal/src/placements.rs` headless per-pane store anchored to stable rows (Model::cursor_anchor, same formula as block anchors), kitty delete selectors (lowercase drop placements, uppercase also free unreferenced image data), scrollback-evicted placements dropped, primary/alternate screen scoping, width reflow invalidation, viewport projection for the renderer; process-quota image eviction reported for cross-pane placement cleanup; a=d enabled, a=T/a=p stay ENOTSUP behind GRAPHICS_DISPLAY_ENABLED until T7. RED: placement API tests failed to compile before implementation; GREEN. Orchestrator checks: fmt ok; clippy -D warnings ok; so1-terminal 145+60+1 pass. Review assess (base 6a88951, committed-only): medium, review_due=false, under_budget -> pending in slice. Risk: none (in-memory bookkeeping, covered by unit tests; a=d enablement was requested).
- [x] **T7 — Textured image rendering** — `08e451c` (codex delegated + parent correction inline): per-pane GPU texture cache, pane-clipped quads with kitty z-order, a=T/a=p enabled, payload-less control commands accepted (no-';' delete). Parent smoke found default cursor movement applied after following text (async decode reordered the stream); fixed inline with OrderedSegments holding PTY segments while a decode is in flight (RED: unresolved OrderedSegments; GREEN). Checks: fmt ok; clippy -D warnings ok; so1-terminal 147+69+1 pass. Linux X11 window-cropped smoke: image render, C=0 cursor after image, C=1 cursor stays, text growth keeps anchors, delete without ';' clears all, d=I with ';' clears one. Not smoked: viewport scrollback scrolling (covered by T6 unit tests). Review assess (base 6a88951, committed-only): medium, review_due=false, under_budget -> slice T6+T7 pending. Risk: none (rendering + in-memory ordering; decode bounds unchanged).
- [x] **T8 — Extract term-core** — `d7fa94d` (codex, delegated): new `crates/term-core` (so1-term-core) holds model, osc, apc, blocks, block_navigation, layout, keyboard, graphics, placements, and renderer projection helpers; so1-terminal re-exports the old paths and keeps PTY management and GPU rendering; PTY-backed model test moved to crates/terminal/src/model_pty_tests.rs. Core has no winit/wgpu/glyphon/cosmic-text/pollster/bytemuck/arboard/portable-pty deps (cargo tree). Orchestrator checks: fmt ok; clippy -D warnings ok; tests 167 core + 20 lib + 30 bin = 217 (215 baseline names all present + 1 re-export test); Linux X11 graphics smoke identical to T7. Native review (review-d7872f616e750c92, T8 alone base 9307223, escalated item 5, granted, lenses risk/readability): approved, acknowledged. Reviewed boundary -> d7fa94d; T6+T7 (medium, under_budget) were not reviewed separately. Follow-up (non-blocking): R2-placement-positional-ctor (term-core/src/placements.rs:31-56). Risk: item 5 (workspace dependency ownership moved to a new crate).
- [x] **T9 — Local control API and CLI** — `aa8871f`: JSON-lines, private socket/named pipe, EventLoopProxy routing, auth and integration tests.
  - Decision 2026-10-28 (user chose "Set agentes (Recomendado)"): v1 commands list (tabs/panes with id, cwd, title), new-tab, split, focus, close, send-text, get-text (visible screen or last block). CLI: `so1ctl` binary in crates/terminal. Orchestrator defaults: protocol types in term-core; one socket per so1term process exported to panes as SO1TERM_SOCKET; peer-uid check in addition to 0700/0600 permissions; Windows named pipe deferred (control API unsupported on Windows in v1, documented gap); SO1TERM_CONTROL=0 disables the socket.
- [ ] **T10 — Persistent session server**: server owns PTYs/core state; startup, detach, shell exit, stop, recovery tests.
- [ ] **T11 — Attach protocol and GUI client**: snapshot/delta, resync, semantic input, image blobs, attach/detach smoke.
- [ ] **T12 — Phase verification**.

## Verification evidence
_(filled per task)_

## Progress / Next step
- Branch created from `main` (`60f2615`). T0-T9 done (T9 `aa8871f`). Next: T10. Reviewed boundary d7fa94d.

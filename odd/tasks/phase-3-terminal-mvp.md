# Phase 3 — Terminal MVP

## Objective
A native terminal emulator (`crates/terminal`) that opens a window, runs `so1sh`
in a pseudo-terminal, emulates VT output and renders it on the GPU, with
keyboard input, scrolling, resize and copy/paste (planing.md §5, Phase 3).

## Problem / Why
so1sh is complete (Phase 2) and already emits OSC 133/OSC 7 marks, but there is
no terminal of our own to consume them. Phase 4 (blocks) builds on this MVP.

## Scope
- In: window, GPU text rendering, PTY, VT emulation, keyboard input, cursor,
  scrollback, resize, mouse selection and clipboard.
- Out: blocks from OSC 133 (Phase 4), panes/tabs (Phase 4), kitty keyboard and
  graphics protocols, remote control, detach/attach (Phase 5), agents (Phase 6).

## Constraints
- Reference code lives OUTSIDE the repo in `~/proyectos/so1Shell-refs/`
  (`rio/` MIT, `alacritty/` Apache-2.0). MIT/Apache code may be studied and
  reused with attribution; never copy GPL/AGPL code (kitty, Warp).
- New crate must build with the pinned nightly (`rust-toolchain.toml`).
- Writer: `codex-impl` (no git add/commit; orchestrator commits). Read-only
  exploration/verification: Claude subagents.
- Branch `feat/phase-3-terminal-mvp`, stacked on `feat/phase-2-modern-shell`.
- GUI behavior is hard to test end to end: keep logic (input encoding, grid
  model, selection) in testable modules with unit tests; GUI smoke runs are
  manual/headless where possible.

## TDD
- Test-first for pure logic (key encoding, selection, resize math, PTY/VT
  plumbing). Rendering/windowing: explained exception + smoke checks.
- Runner: `cargo test --workspace`, `make ci`.

## Delivery
- Strategy: `ask-on-risk`. Forecast: large (new crate); chain strategy still
  to be decided by the user together with Phases 0–2.

## Stack decision (T0, Codex exploration; key claim spot-checked by orchestrator)
- VT/grid: `alacritty_terminal` 0.26.0 (Apache-2.0, vte 0.15) — feed bytes with `vte::ansi::Processor::advance(&mut term, bytes)`; `Term::grid()`, `Term::resize()`.
- Alacritty exposes NO hook for OSC 133/OSC 7 (orchestrator confirmed: no match in `alacritty_terminal/src/event.rs`); Rio's `rio-vt` has `set_semantic_prompt`/`set_current_directory` (`rio-vt/src/performer/handler.rs:1181-1185`) but is a path/workspace crate, not a clean dependency. Decision: alacritty_terminal + our own incremental OSC observer over the raw PTY bytes.
- PTY: `portable-pty` 0.9.0 (MIT) — independent of the UI/VT choice; resize, reader/writer, try_wait/kill.
- Window/text: winit 0.30.13, wgpu 30.0.1, glyphon 0.12.0 + cosmic-text 0.19.0 (MIT/Apache). CPU fallback contingency: softbuffer 0.4.8 (needs own glyph rasterization).
- Clipboard: copypasta 0.10.2 (used by Alacritty) or arboard 3.6.1.
- Verified: a scratch crate with all of the above compiles on nightly-2026-09-15 (25 s). NOT verified: GPU init / window creation at runtime.
- Architecture: UI thread owns `Term`; PTY reader thread sends byte chunks over a channel and wakes the UI; each chunk feeds both the VT processor and the OSC observer; modules window, pty, model, osc, renderer, keyboard, selection, clipboard.

## Platform decision (user, 2026-10-09)
- The terminal targets Linux, macOS and Windows. Design portable from the start: no Linux-only API without a per-platform path; shell command configurable.
- so1sh/epsh are POSIX-only (fork, signals, process groups, termios): default shell is `so1sh` on Linux/macOS; on Windows default to `pwsh` (fallback `cmd`) via ConPTY (portable-pty), or so1sh inside WSL.
- Verification limit: only Linux is available here. macOS/Windows get compile checks (`cargo check --target x86_64-apple-darwin / x86_64-pc-windows-msvc` once those targets are installed); runtime behavior there needs the user's machines or a hosted CI.

## Tasks (refined after T0)
- [x] **T0 — Exploration and stack decision** (Codex, read-only)
- [x] **T1 — Crate skeleton + stack spike** — `35f0b83`: window opens on Linux (adapter Intel UHD Graphics TGL GT1), wgpu clear, close/resize, clear GPU-init errors; unit tests for per-platform shell candidates and surface clamping. Windows/macOS cross-checks skipped: targets not installed. Correction (review): T1 did NOT add or type-check alacritty_terminal — that moves to T4. Native review approved (review-d79f338b8280e79e).: `crates/terminal`, open a window, clear it with wgpu; type-check Term; GPU init failure reported clearly.
- [x] **T2 — OSC observer**: pure incremental parser for OSC 133 A/B/C/D and OSC 7, split at every byte boundary, malformed input (Phase 4 seam).
- [x] **T3 — PTY lifecycle**: spawn `so1sh`, stream output, write input, resize, child exit, cleanup.
- [x] **T4 — VT model adapter**: PTY chunks -> alacritty_terminal; tests for grid contents, cursor, scrollback without a window.
- [x] **T4b — PTY/OSC robustness from the T2-T4 review** (codex-impl) — `35df06b`: Linux session-wide SIGHUP/SIGKILL teardown (so1sh moves jobs to own pgids), bounded reader join, spawn rollback, VTE-matching OSC termination (ESC+non-`\\`, CAN/SUB), prompt test locates so1sh via Cargo env, full-surface background clear. 28 tests, 2/2 runs (orchestrator). Manual X11: closing with `sleep 100 &` exits and leaves no process.
  - Native review (`6a4ff97..5520cee`): high risk, user granted, 4 lenses approved (review-74f7b3c7dea169f3); 7 WARNINGs incl. a PID-reuse signalling risk -> T4c.
  - Known gap (multi-platform): session-wide group discovery is Linux-only (/proc); on macOS only the initial group is signalled, so background jobs may survive close (teardown no longer hangs). Windows relies on portable-pty kill/ConPTY close. Track for T8/Phase 5.
  - R3-pty-drop-join-hang: `Drop` kills/reaps only the direct child and joins the reader thread without a bound; on Unix the reader's cloned master fd only sees EOF/EIO when every process holding the slave tty has closed it, so a background job (e.g. `sleep 100 &`) can hang the terminal on close (`pty.rs:220-232`). Bound the join / close the reader's fd / kill the session's process group.
  - R3-pty-spawn-error-leaks-child: the child is spawned before reader/writer setup; an early `?` return drops the Child without kill/wait (`pty.rs:121-127`).
  - R3-osc-esc-termination-divergence: the observer appends ESC+byte to the payload where VTE ends the OSC (ESC followed by non-`\`) and ignores CAN/SUB aborts, so it can report marks the emulator discarded (`osc.rs:82-111`). Match VTE's termination rules; add tests.
  - R3-model-test-target-dir: the so1sh prompt test hardcodes `target/debug` and runs a nested cargo build ignoring CARGO_TARGET_DIR/profile (`model.rs:263-278`).
- [x] **T5 — Renderer**: draw the visible grid (glyphon), colors, cursor; GPU smoke optional so headless CI stays useful. Also fix (T1 review): when a frame cannot be acquired (Lost/Outdated/Timeout/Occluded) `draw()` never requests another redraw (`crates/terminal/src/main.rs:82-88`).
- [x] **T4c — PTY teardown safety from the T4b review** (codex-impl) — `b30baca`: session id recorded at spawn; groups signalled only if a live member is in that session; reaped pid never reused; sweep works when the shell exits first; early-exit grace loop; logged reader detach; non-vacuous so1sh test; discard-state OSC termination. 37 tests 2/2 (orchestrator) + code read of the session check. Honest TDD note: the unrelated-session survival test did not fail before the fix (invariant test, not RED).
  - R1-001 / R3-pty-teardown-signals-reaped-pid: `terminate_child_groups` calls `try_wait` first (reaping an exited shell) and then still uses that pid for `getsid` and `kill(-pid, SIGHUP/SIGKILL)`; after reaping, the pid/pgid can be REUSED by an unrelated process, which could then be signalled (`pty.rs:532-575`). Capture the session id and process groups BEFORE any reap; never signal a pid/pgid the shell no longer owns.
  - R4: on the normal path (shell exits first) `getsid` on the reaped pid returns ESRCH, the session sweep is skipped, and background jobs survive (`pty.rs:534-548`).
  - R2: the 150 ms SIGHUP grace loop never breaks early; the 100 ms reader-join timeout silently detaches the thread (`pty.rs:496-502, 556-560`): break when done, document/log the detach.
  - R3-so1sh-tests-silently-skip: the real-so1sh prompt test returns as a pass when no binary is found (`model.rs:262-268`) — vacuous; fail or mark ignored with a reason.
  - R3-osc-discard-states-ignore-can-sub: DiscardOsc/DiscardOscEscape states lack the CAN/SUB/ESC termination rules (`osc.rs:102-103`).
- [x] **T5b — Renderer/event-loop fixes from the T5 review** — `0aec752`: bounded exit polling (backoff ≤500 ms), Lost/Outdated redraw once / Timeout delayed retry / Occluded waits for visibility, per-line shaping with reusable buffers, coalesced wakeups; 33 tests 2/2 runs; screenshot `/tmp/so1term-t5b-final.png` (seq 1 5000, no gray strip) inspected; minimized CPU ticks flat. (codex-impl, after T4b)
  - R3-child-exit-race-no-retry: shell exit is checked with one non-blocking `try_wait` on the reader's EOF event; if the slave closes before waitpid reports, the window never closes (`main.rs:418-420`). Poll until the status is available (bounded) or wait in a thread.
  - R3-occluded-redraw-spin: on Timeout/Occluded `draw()` immediately requests another redraw, so a minimized window spins at full CPU reshaping every cell (`main.rs:332-334`). Orchestrator's own T1-review instruction caused this: redraw only on Lost/Outdated (after reconfigure) and on Occluded wait for an occlusion/visibility change.
  - R3-per-frame-full-reshape: every frame shapes a cosmic-text Buffer per cell (incl. blanks) and allocates a new vertex buffer; every PTY chunk triggers a redraw (`main.rs:247-271`). Shape per line or cache, skip blanks, coalesce redraws.
- [ ] **T6 — Input and resize** — also fix R3-input-backspace-gating-and-ctrl (`main.rs:467-491`): input is gated by a local shadow buffer instead of sending keys to the shell; Backspace dropped after history/completion; the encoder must send every key to the PTY.: key-event -> bytes encoder (tested), mouse wheel scrollback, window resize -> PTY + Term.
- [ ] **T7 — Selection and clipboard**: selection model tests, copy/paste, manual GUI smoke.
- [ ] **T8 — Phase verification**

## Carry-over from Phase 2
- Argument-position completion drops broken symlinks (`crates/shell/src/completion.rs:229-232`).
- Suggestions: observable test for integration write failures, fragile plain-highlight assertion, write-then-exec ETXTBSY risk in a PTY test.

## Verification evidence
_(filled per task)_

## Progress / Next step
- T2 done (`a9226d5`): `crates/terminal/src/osc.rs`, absolute end offsets, every-split test (orchestrator checked the expected offsets are derived from sequence lengths, not copied from output). T3 done (`fe39648`): `crates/terminal/src/pty.rs`; 3/3 runs green; tests cover I/O, resize (`stty size`), exit status, drop reaps the child (/proc). Gap closed in T4.
- T4 done (`f0d652a`): `crates/terminal/src/model.rs` (Term + VTE processor + OSC observer on the same bytes, snapshots, scrollback, events incl. DSR write-back); pure shell selection with tests. 17 terminal tests, 2/2 runs green. T5 done (`cf7dcb6`): so1sh runs live in so1term (PTY + model + glyphon renderer, ANSI colors, bold/inverse, block cursor, resize, close/exit wiring, basic text/Enter/Backspace input, redraw on surface errors). Manual Linux check: screenshot `/tmp/so1term-window3.png` shows the green `$` prompt, cursor and red ANSI text (orchestrator inspected it). Runtime resize not verified (no xdotool/wmctrl). UI nit: leftover strip outside the grid painted gray -> T4b.
- Native review of T5 (`c601466..6a4ff97`): medium, user granted, approved (review-00815f686849f9fe); 4 WARNINGs -> T5b and T6.
- T4b running on `codex-impl` (herdr clone in ~/proyectos/so1Shell-refs/herdr as teardown reference).
- Native review T2..T4 (`35f0b83..c601466`): medium, user granted, approved (review-348ac442dcb7b106); 4 WARNINGs -> T4b. Pending user decision: install rustup targets for Windows/macOS compile checks.

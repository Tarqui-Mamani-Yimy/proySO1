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

## Tasks (T1+ preliminary; refined after T0)
- [ ] **T0 — Exploration and stack decision** (read-only Claude explorer)
- [ ] **T1 — `crates/terminal` skeleton: open an empty window**
- [ ] **T2 — Text rendering: font, cell grid, colors**
- [ ] **T3 — PTY: spawn `so1sh` with `portable-pty`**
- [ ] **T4 — VT emulation wired to the PTY output**
- [ ] **T5 — Keyboard input to the PTY (incl. Ctrl-C/Ctrl-Z)**
- [ ] **T6 — Cursor, scrollback and resize**
- [ ] **T7 — Mouse selection, copy and paste**
- [ ] **T8 — Phase verification**

## Carry-over from Phase 2
- Argument-position completion drops broken symlinks (`crates/shell/src/completion.rs:229-232`).
- Suggestions: observable test for integration write failures, fragile plain-highlight assertion, write-then-exec ETXTBSY risk in a PTY test.

## Verification evidence
_(filled per task)_

## Progress / Next step
- T0 running.

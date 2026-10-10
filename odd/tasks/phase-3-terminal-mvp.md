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

## Tasks (refined after T0)
- [x] **T0 — Exploration and stack decision** (Codex, read-only)
- [ ] **T1 — Crate skeleton + stack spike**: `crates/terminal`, open a window, clear it with wgpu; type-check Term; GPU init failure reported clearly.
- [ ] **T2 — OSC observer**: pure incremental parser for OSC 133 A/B/C/D and OSC 7, split at every byte boundary, malformed input (Phase 4 seam).
- [ ] **T3 — PTY lifecycle**: spawn `so1sh`, stream output, write input, resize, child exit, cleanup.
- [ ] **T4 — VT model adapter**: PTY chunks -> alacritty_terminal; tests for grid contents, cursor, scrollback without a window.
- [ ] **T5 — Renderer**: draw the visible grid (glyphon), colors, cursor; GPU smoke optional so headless CI stays useful.
- [ ] **T6 — Input and resize**: key-event -> bytes encoder (tested), mouse wheel scrollback, window resize -> PTY + Term.
- [ ] **T7 — Selection and clipboard**: selection model tests, copy/paste, manual GUI smoke.
- [ ] **T8 — Phase verification**

## Carry-over from Phase 2
- Argument-position completion drops broken symlinks (`crates/shell/src/completion.rs:229-232`).
- Suggestions: observable test for integration write failures, fragile plain-highlight assertion, write-then-exec ETXTBSY risk in a PTY test.

## Verification evidence
_(filled per task)_

## Progress / Next step
- T0 done. Open product question for the user: target platforms for the MVP. Then T1 on `codex-impl`.

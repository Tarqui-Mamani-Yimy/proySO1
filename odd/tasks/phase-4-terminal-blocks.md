# Phase 4 — Terminal with blocks

## Objective
Turn so1term into a block-based terminal: every command with its output is a
unit built from the shell's OSC 133 marks, navigable, copyable and marked by
exit status; then add panes and tabs (planing.md §5, Fase 4).

## Problem / Why
Phase 3 delivered a classic grid terminal. The value of so1term over any other
terminal is the shell/terminal contract: so1sh already emits OSC 133 A/B/C/D
and OSC 7, and `crates/terminal/src/osc.rs` already observes them with absolute
byte offsets. Phase 4 consumes those marks.

## Scope
- In: block model from OSC 133 (prompt, input, output, exit status, cwd),
  block rendering (separators, error marking), block navigation and copy,
  coexistence with the alternate screen (vim, less, top), panes (splits) and
  tabs, each pane with its own PTY.
- Out: kitty keyboard/graphics protocols, remote control, persistent server
  (Phase 5); agent features (Phase 6).

## Constraints
- Platforms: Linux, macOS, Windows (code must stay portable; runtime checks on Linux).
- Licenses: WezTerm (MIT), Rio (MIT), herdr (Apache-2.0) may be studied and
  reused with attribution; Warp (AGPL) and kitty (GPL) are ideas/spec only.
- Must work with shells that emit no OSC 133 (graceful fallback to a plain grid).
- Single writer: `codex-impl`. Orchestrator commits (Codex git ops are denied).
- Branch `feat/phase-4-blocks`.

## TDD
- Mode: strict. Runner: `cargo test -p so1-terminal < /dev/null`, `make ci`.
- Block model, navigation and pane layout are pure and unit-tested; GUI behavior
  gets a Linux smoke with screenshots.

## Delivery
- Strategy: `ask-on-risk`. Forecast: well over 400 authored lines -> native
  review per work-unit slice as `review_due` requires.

## References
- `~/proyectos/so1Shell-refs/wezterm` (semantic zones from OSC 133, mux panes/tabs).
- `~/proyectos/so1Shell-refs/rio` (splits/tabs), `~/proyectos/so1Shell-refs/herdr` (panes, PTY per pane).

## Tasks
- [ ] **T0 — Design spike** (codex-impl, read-only): study WezTerm semantic zones and mux, our `osc.rs`/`model.rs`; decide block storage (one grid per block, as planing.md suggests from Warp, vs. zones over the shared alacritty grid + scrollback), alternate-screen handling, and pane/tab architecture. Output: decision with tradeoffs, file:line evidence, revised task list.
- [ ] **T1 — Block model**: OSC 133 events -> blocks (prompt/input/output ranges, exit status, cwd); pure tests incl. missing marks, Ctrl-C cancelled prompts, multiline PS2, no-OSC-133 shells.
- [ ] **T2 — Block rendering**: separators/gutter, error marking by exit status.
- [ ] **T3 — Block navigation and copy**: jump between blocks, select a block, copy its output/command.
- [ ] **T4 — Alternate screen coexistence**: TUI programs do not corrupt blocks.
- [ ] **T5 — Panes**: splits, focus, one PTY per pane, resize.
- [ ] **T6 — Tabs**.
- [ ] **T7 — Phase verification**.

## Verification evidence
_(filled per task)_

## Progress / Next step
- Branch created from `main` (`1965232`). Next: T0 on codex-impl.

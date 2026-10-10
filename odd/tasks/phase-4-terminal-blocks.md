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

## Decision (T0, user-approved 2026-10-10)
- Blocks are metadata ranges over one `alacritty_terminal::Term` per pane (WezTerm semantic-zone model), not one VT grid per block. Supersedes planing.md's "una grilla VT por bloque": splitting one PTY stream across emulators breaks cursor addressing, redraws and screen switches.
- OSC byte offsets only order events; grid anchors come from feeding the model in segments that end at each OSC mark and snapshotting cursor + active screen.
- Alternate screen: primary grid and blocks preserved; TUI redraws are never anchored to primary rows; a block finalizes at the shell's D mark.
- Panes/tabs: a Pane owns PTY + Model + observer + blocks + view state; a Tab owns a pure split tree and the active pane; one window and one wgpu renderer draw each pane in its rectangle; PTY wake events carry a pane id.
- Main risk: anchor stability across reflow/scrollback eviction — T2 is a gate before block UI.
- Evidence (Codex spike): wezterm-cell/src/lib.rs:176-214, term/src/terminalstate/performer.rs:867-906, term/src/terminalstate/mod.rs:152-158,2822-2871, mux/src/tab.rs:17-130; our osc.rs:23-62, model.rs:56-113.

## Tasks
- [x] **T0 — Design spike** (codex-impl, read-only) — decision above.
- [x] **T1 — Block state model** — `9417532` (codex-impl, delegated): `blocks.rs` BlockList consumes observer events in Model::feed; states Prompt/Input/Running/Finished/Cancelled; cancel on A after A/B; tolerant of missing/duplicate/malformed marks; no blocks without OSC 133; GridAnchors slots for T2. RED observed (7 failures, no-op consumer); 83+2 tests 2/2 (orchestrator), fmt/clippy clean. Known follow-up: block list unbounded (eviction in T2). Scope: pure OSC 133/OSC 7 state machine -> block records (prompt/input/output, exit status, cwd); missing marks, cancelled input, multiline input, no-OSC fallback; table-driven tests.
- [ ] **T2 — Grid checkpoints and anchors** (gate): segment feeding at OSC boundaries, cursor + screen snapshot, anchors rebased through scrolling, resize/reflow and eviction; headless tests (multiple marks per chunk, split sequences, wrapping, scrollback, resize, eviction).
- [ ] **T3 — Block rendering**: separators, regions, cwd, non-zero exit marking as overlays; pure projection tests.
- [ ] **T4 — Block navigation and copy**: previous/next prompt, select block, copy command/output; pure tests + GUI smoke.
- [ ] **T5 — Alternate-screen coexistence**: primary anchors survive TUI enter/exit; block finalizes on D.
- [ ] **T6 — Pane layout and focus model**: pure split tree, geometry, hit-testing, focus, minimum sizes.
- [ ] **T7 — PTY panes, tabs and phase verification**: one PTY/model per pane, multi-viewport render, tabs, input routing, resize; GUI smoke with two panes and two tabs.

## Verification evidence
_(filled per task)_

## Progress / Next step
- Branch created from `main` (`1965232`). T0 done, decision approved by the user. T1 done. Next: T2 (anchor gate).

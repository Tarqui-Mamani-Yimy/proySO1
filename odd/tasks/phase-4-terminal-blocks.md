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
- [x] **T2 — Grid checkpoints and anchors** (gate) — `34a01b7` (codex-impl): segmented feed at OSC end offsets; anchors = evicted_rows + history_size + cursor_line; EvictionTracker proxies vte Handler (orchestrator verified all 71 methods delegated) and counts rows pushed past the history limit (DECSTBM- and alt-screen-aware, wrap included); blocks expire only when their rows are evicted; bounded BlockList (1024); phantom D ignored; implicit end boundary recorded. Orchestrator rejected the first draft (expired all anchors on any full damage once history was full). RED->GREEN recorded; 99+2 tests 2/2 (orchestrator). Policy/gap: resize invalidates all anchors (no reliable reflow mapping in alacritty 0.26). Original scope — also fold the review advisories of the T8+T1 slice (review-e34e8ab8efda0fb7, approved): R3-command-finished-without-open-block-creates-phantom-block (`blocks.rs:133-139`), R3-running-block-implicitly-finished-without-end-boundary (`blocks.rs:84`), R3-block-list-unbounded-growth (`model.rs:115`), R3-offscreen-selection-string-compare-per-feed (`model.rs:149-155`), R3-backward-anchor-rebase-untested (`model.rs:137-146`). Scope: segment feeding at OSC boundaries, cursor + screen snapshot, anchors rebased through scrolling, resize/reflow and eviction; headless tests (multiple marks per chunk, split sequences, wrapping, scrollback, resize, eviction).
- [x] **T3 — Block rendering** — `3c89740` (codex-impl): T2 advisories fixed RED->GREEN (clear-saved counts evicted rows, clear-all/reset invalidate, resize-invalidated blocks survive eviction, wide-char wrap predicted before feed); pure viewport projection; separators + state markers drawn after glyphs inside the first cell's padding (grid width unchanged); none on alt screen / unanchored / no OSC 133. Orchestrator rejected the first RC-driven smoke; real typed smoke `/tmp/so1term-p4t3-real-{blocks,cancelled,scrolled}.png` inspected: red separator for `false`, grey cancelled mark, blue active prompt. 106+4 tests, fmt/clippy clean. Gap: exit code label omitted; markers are small. Original: first fold the T2 review advisories (review-0b78a86f95f71327, approved): R3-clear-screen-and-history-reset-bypass-eviction-tracking (`model.rs:186-222`), R3-resize-then-eviction-drops-all-completed-blocks (`blocks.rs:202-219`), R3-wide-char-wrap-eviction-check-reads-post-scroll-row (`model.rs:140-146`). Then: separators, regions, cwd, non-zero exit marking as overlays; pure projection tests.
- [x] **T4 — Block navigation and copy** — `69ff573` (codex-impl): T3 advisories fixed RED->GREEN (monotonic stable rows across ED2/reset, pending-wrap-based wide-char eviction, clear-all projection test); `block_navigation.rs`: Ctrl+Shift+Up/Down (Cmd+Up/Down) prompt jumps with top alignment, Ctrl+Shift+A (Cmd+Shift+A) select block output, Ctrl+Shift+O / Ctrl+Shift+L copy output / command; copy refuses evicted or alt-screen ranges. 115+5 tests 2/2 (orchestrator). Smoke inspected: `/tmp/so1term-p4t4-nav-up.png`, `/tmp/so1term-p4t4-selected.png`. Gaps: clipboard contents not read (no xclip/xsel); multiline printf not smoke-verified. Original: first fold the T3 review advisories (review-e093c3e818e9b6de, approved): R3-ed2-and-reset-make-evicted-rows-non-monotonic (`model.rs:210-214`), R3-wide-wrap-prediction-depends-on-width-parity (`model.rs:138-140`), R3-clear-all-epoch-projection-untested (`model.rs:971-982`). Then: previous/next prompt, select block, copy command/output; pure tests + GUI smoke.
- [x] **T5 — Alternate-screen coexistence** — `183fe04` (codex-impl): T4 advisories fixed RED->GREEN (new prompt releases navigation pin; ED2 invalidates erased anchors without cancelling blocks; block shortcuts pass through in alt screen; alt-mark refusal after exit tested). Alt screen: primary anchors and Running survive TUI output, D finalizes after return, decorations hidden/restored; DECSET 47/1047 normalized to 1049 because alacritty 0.26 only handles 1049 (note: adds cursor save/restore and clear semantics to 47/1047 apps). 120+6 tests 2/2 (orchestrator). Smoke inspected `/tmp/so1term-p4t5-after.png` (echo, less, true decorations intact). Original: first fold the T4 review advisories (review-22a340499ad27f28, approved): R3-pinned-viewport-never-returns-to-live-follow (`model.rs:788-800`), R3-ed2-cancels-running-and-prompt-blocks (`blocks.rs:228-233`, e.g. the `clear` command must not cancel its own block), R3-block-shortcuts-swallow-keys-in-alternate-screen (`main.rs:905-910`), R3-alternate-mark-refusal-only-tested-while-alt-active (`model.rs:1315-1318`), R3-cursor-offscreen-sentinel-unverified (`model.rs:804-811`). Then: primary anchors survive TUI enter/exit; block finalizes on D.
- [x] **T6 — Pane layout and focus model** — `96eaa81` (codex-impl): `layout.rs` split tree, cell-snapped rects, minimum-size refusal, hidden panes on shrink, divider hit-test, directional focus (tie policy in AGENTS.md), close collapsing, ratio clamping, Tab/TabSet. RED (compile) -> GREEN; 128+6 tests 2/2 (orchestrator). Note for T7: PaneId is unique per tree only — must be globally unique when PTY events carry it. Original: pure split tree, geometry, hit-testing, focus, minimum sizes.
- [x] **T7a — PTY panes in the window** — `86995ab` (codex-impl, ~1425 changed lines: exceeded the 400 heuristic because main.rs moved from one PTY/model to a pane map; kept as one coherent unit): global atomic PaneId allocator (RED: both trees allocated PaneId(1)); Pane {PTY, Model, view state, glyph cache} map; per-pane wakeups (PaneId in proxy event, bytes drained from each pane's channel); clipped per-pane rendering, tree-derived dividers, focused border; Ctrl+Shift+D/E split, Ctrl+Shift+W close, Ctrl+Alt+Arrows focus. R3-001 did not reproduce (layout computed on demand; shrink/restore test kept). 131+9 tests 2/2 (orchestrator). Smoke inspected `/tmp/so1term-p4t7a-three-final.png`, `-resized-final2.png`. Gap: block decorations vanish after resize (T2 policy). Original: make PaneId globally unique across tabs; fold T5+T6 review advisory R3-001 (`layout.rs:159-161`, resize_window stores the rect without revalidating minimum sizes/hidden panes); one PTY+Model per pane, multi-viewport render, focus routing, per-pane resize, pane id in PTY wake events, close pane on shell exit.
- [x] **T7b — Tabs and phase verification** — `2cabd24` (codex-impl, ~850 changed lines): T7a advisories fixed (shared close lifecycle resizes siblings on shell exit; drag ends in its origin pane; lifecycle helpers tested; hidden-tab exit redraws the tab bar). TabSet in the window, one-row tab bar (hidden with a single tab, cwd basename labels), Ctrl+Shift+T/Q, Ctrl+Tab/Ctrl+Shift+Tab, Alt+1..9 (Cmd on macOS); hidden tabs drain output without redraw. `make ci`: fmt, clippy, build, workspace tests PASS; conformance 167/168 (regression-37). Terminal tests 2/2 (orchestrator). Smoke inspected `/tmp/so1term-p4t7b-hidden-tab-output.png` (hidden-tab seq 1 500 arrived).
  - Acceptance vs planing.md §5 Fase 4: blocks from OSC 133 PASS (metadata over shared grid, user-approved deviation); navigate/copy blocks PASS (clipboard not read back); error marking by exit status PASS; panes and tabs PASS.
  - Gaps: resize invalidates block anchors; active tab highlight is faint; macOS/Windows not compiled; clipboard contents not read back. Original: first fold the T7a review advisories (review-8230b1bcd3ce8cb8, approved): R3-shell-exit-close-skips-sibling-resize (`main.rs:562`), R3-selection-release-lost-across-panes (`main.rs:1393-1403`), R3-pane-lifecycle-paths-untested (`main.rs:546-550`). Then tab bar, new/close/next/prev, hidden tabs keep draining PTYs without redraw spins; full phase verification. Original combined scope: one PTY/model per pane, multi-viewport render, tabs, input routing, resize; GUI smoke with two panes and two tabs.
- [ ] **T8 — Follow-ups from the T7b review** (review-9e2f272f7bc0adc9, approved; non-blocking): R3-alt-digit-swallowed-with-single-tab (`main.rs:188-191`, Alt+1..9 must reach the shell when no tab exists for that index), R3-new-tab-spawn-failure-leaves-orphan-tab (`main.rs:804-819`), R3-osc-window-title-events-dropped (`main.rs:1564`), R3-keyboard-tab-switch-forces-full-resize (`main.rs:878-880`), R3-osc7-cwd-title-derivation-untested (`main.rs:1545-1558`); plus a clearer active-tab highlight.

## Verification evidence
_(filled per task)_

## Progress / Next step
- Branch created from `main` (`1965232`). T0 done, decision approved by the user. Phase 4 T0-T7b done and reviewed; T8 follow-ups open. Next: user decides merge to main; then Phase 5.

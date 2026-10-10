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

## Tasks
- [ ] **T0 — Design spike** (codex-impl, read-only): order and architecture for the four features; how alacritty_terminal 0.26 / vte 0.15 handle kitty keyboard modes and APC graphics sequences; image storage and rendering in our wgpu pipeline; control-socket protocol and transport per platform; server/client split (what moves to the server, wire protocol, reattach, Windows story). Output: decision with tradeoffs, evidence, revised task list.

## Verification evidence
_(filled per task)_

## Progress / Next step
- Branch created from `main` (`60f2615`). Next: T0 on codex-impl.

# Phase 2 — Modern Shell

## Objective
Turn so1sh into a modern interactive shell: real job control, shell
integration marks for block-aware terminals, completion, hints and syntax
highlighting (planing.md §5, Phase 2).

## Problem / Why
Phase 1 discards stopped jobs and has no `jobs`/`fg`/`bg`; terminals cannot
see command boundaries; there is no completion or highlighting. Phase 1 also
left small follow-ups (see Phase 1 result in `odd/tasks/phase-1-shell-mvp.md`).

## Scope
- In: Phase 1 follow-ups; embedder builtin hook in epsh; job table, `jobs`,
  `fg`, `bg`, Ctrl-Z keeping jobs, done/stopped notifications; OSC 133 + OSC 7;
  tab completion; history hints; syntax highlighting.
- Out: terminal emulator (Phase 3+), kitty keyboard/graphics protocols, agents.

## Architecture decision (orchestrator, 2026-10-06)
- epsh gains a generic, backward-compatible **embedder builtin hook** (the
  application registers builtins that run inside the shell, before PATH lookup).
  Job-control UI (`jobs`/`fg`/`bg`, notifications) lives in `crates/shell`.
  Why: keeps the engine minimal and embeddable (agents, future terminal), while
  the engine keeps owning process groups and wait status.

## Constraints
- epsh public API stays backward compatible (`api_stability`).
- Interactive behavior tested end to end through a PTY; probes must answer
  reedline's `ESC[6n` cursor query.
- Writer: `codex-impl` (writes/verifies, cannot git add/commit); orchestrator
  reviews diffs, spot-checks and commits. Verifier: fresh Claude subagent.
- Branch `feat/phase-2-modern-shell`, stacked on `feat/phase-1-shell-mvp`.
- OSC 133/OSC 7 implemented from public specs only (no GPL/AGPL code).

## TDD
- Mode: strict (user global config). Runner: `cargo test --workspace`, `make ci`.

## Delivery
- Strategy: `ask-on-risk`. Forecast: ~1500–2500 authored lines -> chained PRs;
  chain strategy still to be decided by the user together with Phases 0–1.

## Tasks
- [x] **T0 — Phase 1 follow-ups** (codex-impl)
  - Relative `SO1SH_RC` without `/` must not be searched in PATH (`repl.rs:90`).
  - Runtime error positions print `0:0` (e.g. `epsh: 0:0: nope: missing`): report real line:col or drop the prefix.
  - Remove dead `wifstopped` check (`eval.rs:860`); clear the sticky `special_builtin_failed` marker reliably (`eval.rs:289`).
- [ ] **T0b — T0 review follow-ups** (codex-impl, small, after T1)
  - R3-sticky-flag-ok-path: `special_builtin_failed` is only cleared on error arms; a non-fatal special-builtin failure (e.g. `command break foo`) inside one compound command leaves it set, so a following `exit N` in the same command (`{ command break foo; exit 5; }`) is misclassified. Clear it when the explicit `exit` builtin runs and on the Ok path; add that exact test.
  - R3-relative-rc-test: make the PTY test prove no PATH search (same-named rc in a PATH dir must never be sourced).
- [ ] **T1 — Embedder builtin hook in epsh** (codex-impl)
  - Register builtins by name with a callback receiving the shell and argv; runs in-process with redirections applied; listed by `command -v`/`type`; does not override POSIX special builtins.
  - Accept: embedding + api_stability tests; existing behavior unchanged.
- [ ] **T2 — Job table and `jobs`** (codex-impl)
  - Ctrl-Z keeps the stopped job (no discard); `cmd &` registers a job; `jobs` lists `[n]+ Running/Stopped/Done  cmd`; `%n`, `%+`, `%-`, `%string` job specs.
- [ ] **T3 — `fg` and `bg`** (codex-impl)
  - `fg %n` gives the job the terminal, SIGCONT to the group, waits, restores modes; `bg %n` resumes in background; status codes per POSIX.
  - Fix: SIGTTIN acquisition deadline must not count time spent stopped (`terminal.rs:74-79`).
- [ ] **T4 — Job notifications** (codex-impl)
  - Before each prompt report `[n]+ Done/Stopped/Killed` for changed background jobs; `exit` with stopped jobs warns once (like bash).
- [ ] **T5 — Shell integration marks** (codex-impl)
  - Emit OSC 133 A/B/C/D (prompt start, command start, output start, command end with exit code) and OSC 7 (cwd as file:// URL) when interactive; opt-out variable.
  - Accept: PTY tests assert exact sequences and ordering.
- [ ] **T6 — Tab completion** (codex-impl)
  - Commands (builtins, functions, embedder builtins, PATH executables), file paths (quoting spaces), `$VAR` names.
- [ ] **T7 — Hints and syntax highlighting** (codex-impl)
  - History-based ghost-text hints; highlighting via the epsh lexer (commands found/not found, strings, operators).
- [ ] **T8 — Phase verification** (fresh Claude verifier subagent)

## Verification evidence
- T0 (`09567a4` runtime errors omit unknown `0:0` positions; `fb0693b` clear REPL failure marker on handled errors, remove dead WIFSTOPPED checks; `9b082b2` PTY test: slash-less `SO1SH_RC` resolves in cwd — Codex refuted the T5 review claim of PATH search with evidence `builtins.rs:418`). Workspace 557 green (orchestrator spot-check).
  Native review: high risk, user granted, approved, acknowledged. WARNINGs -> T0b. Process note: the new Codex session tried to run native review itself; told that review is orchestrator-owned.

## Progress / Next step
- Codex re-briefed (new session, gpt-6-luna). T1 running on `codex-impl`; then T0b, T2.

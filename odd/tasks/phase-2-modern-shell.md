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
- [x] **T0b — T0 review follow-ups** (codex-impl, small, after T1)
  - R3-sticky-flag-ok-path: `special_builtin_failed` is only cleared on error arms; a non-fatal special-builtin failure (e.g. `command break foo`) inside one compound command leaves it set, so a following `exit N` in the same command (`{ command break foo; exit 5; }`) is misclassified. Clear it when the explicit `exit` builtin runs and on the Ok path; add that exact test.
  - R3-relative-rc-test: make the PTY test prove no PATH search (same-named rc in a PATH dir must never be sourced).
- [x] **T1 — Embedder builtin hook in epsh** (codex-impl)
  - Register builtins by name with a callback receiving the shell and argv; runs in-process with redirections applied; listed by `command -v`/`type`; does not override POSIX special builtins.
  - Accept: embedding + api_stability tests; existing behavior unchanged.
- [ ] **T0c — Replace the sticky special-builtin failure flag** (codex-impl, with T1b)
  - Third review in a row flags `special_builtin_failed` as a stale global side channel (T0b review R3/R4: the `command` arm converts any Exit to MISUSE while the flag is stale, and never resets it before dispatch or after conversion, `builtins.rs:675-682`).
  - Redesign: carry the "recoverable special-builtin failure" cause in the error itself (private error variant or wrapper) instead of a shell-wide flag; remove the flag.
- [ ] **T1b — Builtin hook follow-ups from review** (codex-impl, after T2)
  - R3-sink-vs-redirection: `write_stdout`/`write_stderr` prefer the sink even when the command's stdout is redirected (`hello > file` with a sink set writes to the sink, not the file); untested. Define and test the rule (redirection wins).
  - R4: a panicking embedder callback skips fd restoration (`eval.rs:1195`); add an unwind guard.
  - R3: functions now take precedence over regular epsh builtins (POSIX-correct, but a behavior change for scripts wrapping `cd`/`echo`); document it in AGENTS.md "Where we diverged"/changelog and add a test.
  - R3/R2: special-builtin names without an epsh implementation (e.g. `times`) now fail with CommandNotFound instead of falling back; decide and test; simplify the double dispatch.
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

- T1 (`0464e73`): `Shell::register_embedder_builtin` (+ explicit regular-builtin shadowing API, never special builtins), byte argv, sink-aware `write_stdout/stderr`, `command -v`/`type` discovery; lookup special -> functions -> epsh builtins -> embedder -> PATH. Also fixed: command-substitution children kept an inherited stdout sink, so builtin output bypassed the capture pipe. Workspace 568 green.
  Native review: high risk, user granted, approved, acknowledged. 5 WARNINGs -> T1b.
- T0b review: high risk, user granted, approved, acknowledged; WARNINGs -> T0c.
- T0b (`c28731d`): `command` turns special-builtin usage failures into an ordinary status (POSIX/dash), so `{ command break foo; exit 5; }` exits 5; marker cleared by explicit exit and on success; relative-rc PTY test now has a PATH decoy. Workspace 571 green.

## Progress / Next step
- T2 running on `codex-impl`; T0b review pending; then T1b, T3.

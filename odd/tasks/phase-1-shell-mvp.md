# Phase 1 — Shell MVP

## Objective
A usable interactive shell binary on top of epsh: prompt, line editing with
persistent history, foreground job handling with correct Ctrl-C behavior, and a
startup rc file (planing.md §5, Phase 1).

## Problem / Why
epsh is a non-interactive engine: run with no arguments on a TTY it prints usage
and exits. Users need a REPL. Engine prerequisite: the `wait` builtin reaps any
child of the host process, which is unsafe once epsh is embedded in our shell.

## Scope
- In: `wait` fix in epsh; new crate `crates/shell` (working binary name `so1sh`,
  renameable — final name is an open decision in planing.md §8); REPL; reedline
  editor with history and multiline input; foreground process group and signal
  handling; rc file.
- Out: job control builtins (`jobs`/`fg`/`bg`, Ctrl-Z) and OSC 133 blocks
  (Phase 2), completion and highlighting (Phase 2), terminal emulator (Phase 3+).

## Constraints
- epsh public API stays backward compatible (`tests/api_stability.rs`).
- Interactive behavior is tested end to end through a pseudo-terminal (PTY),
  not only unit tests.
- Writer: `codex-impl` (writes and verifies, cannot git add/commit);
  orchestrator reviews diffs, spot-checks and commits each work unit.
  Verifier: `agy-research` (read-only).
- Branch `feat/phase-1-shell-mvp`, stacked on `chore/phase-0-workspace`
  (Phase 0 not yet merged into `main`).
- No GPL/AGPL code copied (kitty, Warp): specs and ideas only.

## TDD
- Mode: strict (source: user global config). Runner: `cargo test --workspace`,
  plus `make ci`.
- REPL/terminal behavior: RED/GREEN through PTY-driven integration tests.

## Delivery
- Strategy: `ask-on-risk` (default). Forecast: ~800–1200 authored lines
  (new crate + tests) -> exceeds the ~400-line budget; chain strategy to be
  chosen by the user before the PR is planned.

## Tasks

- [x] **T1 — `wait` only reaps the shell's own children** (route: delegated → `codex-impl`)
  - `crates/epsh/src/builtins.rs` `builtin_wait` uses `waitpid(-1)` and ignores its arguments.
  - Track background child PIDs in the shell; `wait` waits for those only; support `wait <pid>` with POSIX exit status.
  - Accept: RED test where a host child survives `wait`; GREEN; api_stability unchanged.
- [ ] **T1b — Parser: several `&` lists on one line** (route: delegated → `codex-impl`; found during T1)
  - `crates/epsh/src/parser.rs:261-271` mishandles `a & b &` on the same line; also `sys.rs:103-108` maps negative PIDs incorrectly.
  - Accept: RED integration tests for `sleep 0.1 & sleep 0.1 & wait; echo done` and two `$!` values; GREEN.
- [x] **T2 — `crates/shell` skeleton + minimal REPL** (route: delegated → `codex-impl`)
  - New workspace member, binary `so1sh`. Loop: print prompt, read a line, run it with epsh, keep `$?`, exit on `exit` and on EOF (Ctrl-D).
  - Non-TTY stdin keeps epsh script semantics.
  - Accept: PTY test types `echo hi`, sees `hi` and a new prompt; `exit 3` returns status 3.
- [ ] **T2b — Interactive shells survive shell errors** (route: delegated → `codex-impl`; review WARNINGs on T2)
  - `eval_repl_line` returns `Break` for generic runtime errors and Cancelled/Stopped (`crates/epsh/src/eval.rs:930-942`), so e.g. `${x?msg}` or a special-builtin error ends the session. POSIX 2.8.1: an interactive shell must not exit on these errors.
  - Only `exit` (and errexit when set) should end the REPL; errors set `$?` and return to the prompt. Document the `Break` contract.
  - Accept: PTY tests for `${nope?missing}` then `echo after`, and a redirection error on a special builtin.
- [ ] **T3 — Line editor with persistent history** (route: delegated → `codex-impl`)
  - reedline: history file, Ctrl-R search, multiline input when the parser reports incomplete input (open quote, `if` without `fi`).
  - Accept: PTY tests for history recall across sessions and a multiline `if`.
- [ ] **T4 — Foreground jobs and signals** (route: delegated → `codex-impl`)
  - Shell ignores SIGINT/SIGQUIT/SIGTSTP/SIGTTOU at the prompt; children run in their own process group with the terminal (epsh `interactive(true)`).
  - Ctrl-C kills the foreground command, not the shell; status 130.
  - Accept: PTY test `sleep 100` + Ctrl-C returns to the prompt with `$?` 130.
- [ ] **T5 — Startup rc file** (route: delegated → `codex-impl`)
  - Source `$SO1SH_RC` or `~/.so1shrc` at startup when interactive; errors reported, not fatal.
  - Accept: PTY test with an rc that sets PS1 and an alias-free function.
- [ ] **T6 — Phase verification** (route: delegated → `agy-research`)
  - `make ci` + manual smoke script through a PTY; report.

## Verification evidence
- T1 (`57611af`): background job PIDs tracked per shell, cleared in forked children; `wait` waits only for those; `wait <pid>` returns its status, 127 if unknown. Tests 147/16/50/239/4 green (orchestrator spot-check, stdin=/dev/null); make ci only regression-37.
  Native review: high risk, user granted, approved, acknowledged. Advisory: EINTR retry makes `wait` non-interruptible by trapped signals (POSIX expects >128) -> Phase 2 traps work; operand validation and unknown-PID diagnostics are silent.

- T2 (`dc389bc` epsh: `epsh::cli` shared dispatch + `Shell::eval_repl_line`/`finish_repl`; `05d551a` so1sh crate): REPL with PS1, state persists, `exit n`/EOF statuses; 12 PTY + 5 CLI tests (rexpect 0.7.1, MIT/Apache-2.0, dev-only). Workspace 473 green (orchestrator spot-check).
  Native review: high risk, user granted, approved, acknowledged. WARNINGs -> T2b (errors end the REPL); SIGINT kills the shell -> T4. Suggestions: hardcoded prefix `cli.rs:19`, API surface addition not covered by api_stability.

## Progress / Next step
- Next: T1b running on `codex-impl`; then T2b, T3.

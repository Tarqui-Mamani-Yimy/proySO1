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
- [x] **T1b — Parser: several `&` lists on one line** (route: delegated → `codex-impl`; found during T1)
  - `crates/epsh/src/parser.rs:261-271` mishandles `a & b &` on the same line; also `sys.rs:103-108` maps negative PIDs incorrectly.
  - Accept: RED integration tests for `sleep 0.1 & sleep 0.1 & wait; echo done` and two `$!` values; GREEN.
- [x] **T2 — `crates/shell` skeleton + minimal REPL** (route: delegated → `codex-impl`)
  - New workspace member, binary `so1sh`. Loop: print prompt, read a line, run it with epsh, keep `$?`, exit on `exit` and on EOF (Ctrl-D).
  - Non-TTY stdin keeps epsh script semantics.
  - Accept: PTY test types `echo hi`, sees `hi` and a new prompt; `exit 3` returns status 3.
- [x] **T2b — Interactive shells survive shell errors** (route: delegated → `codex-impl`; review WARNINGs on T2)
  - `eval_repl_line` returns `Break` for generic runtime errors and Cancelled/Stopped (`crates/epsh/src/eval.rs:930-942`), so e.g. `${x?msg}` or a special-builtin error ends the session. POSIX 2.8.1: an interactive shell must not exit on these errors.
  - Only `exit` (and errexit when set) should end the REPL; errors set `$?` and return to the prompt. Document the `Break` contract.
  - Accept: PTY tests for `${nope?missing}` then `echo after`, and a redirection error on a special builtin.
- [x] **T3 — Line editor with persistent history** (route: delegated → `codex-impl`)
  - reedline: history file, Ctrl-R search, multiline input when the parser reports incomplete input (open quote, `if` without `fi`).
  - Accept: PTY tests for history recall across sessions and a multiline `if`.
- [x] **T3b — Line editor follow-ups from review** (route: delegated → `codex-impl`, after T4)
  - R3-001: `parse_interactive` treats any syntax error after EOF as incomplete; `echo >` + Enter waits for continuation instead of reporting `syntax error near newline` (orchestrator PTY probe was INVALID: it did not answer reedline's ESC[6n cursor query; the bug was independently proven by Codex's RED tests and the dash comparison). Fix and test.
  - R3-002: no tests for history I/O failure paths (unwritable or directory `SO1SH_HISTFILE`).
  - R3-003: treat empty `HOME` / `SO1SH_HISTFILE` as unset.
  - R3-004/R3-005 (suggestions): empty heredoc delimiter needs two Enters; Ctrl-R test does not prove submission semantics.
- [x] **T4 — Foreground jobs and signals** (route: delegated → `codex-impl`)
  - Shell ignores SIGINT/SIGQUIT/SIGTSTP/SIGTTOU at the prompt; children run in their own process group with the terminal (epsh `interactive(true)`).
  - Ctrl-C kills the foreground command, not the shell; status 130.
  - Accept: PTY test `sleep 100` + Ctrl-C returns to the prompt with `$?` 130.
- [x] **T4b — Signal follow-ups from review** (route: delegated → `codex-impl`, after T3b; HIGH priority)
  - R3-001 (orchestrator PTY probe was INVALID — unanswered ESC[6n; bug independently proven by Codex's RED 0/4): Ctrl-C kills the child but does not abort the enclosing loop/list; `while true; do sleep 0.3; done` cannot be interrupted and the shell never returns to the prompt. A foreground child killed by SIGINT must abort the current line (like dash/bash).
  - R4-001: `Terminal::new` calls tcsetpgrp without checking it is in the foreground (e.g. `so1sh &` steals the terminal); wait until foreground (SIGTTIN loop) and remember the original foreground group.
  - R3-002: if `Terminal::new` fails (no controlling tty), fall back gracefully instead of failing the REPL.
  - R2-003: interactive wait path bypasses cancel/timeout-aware wait and skips joining relay threads.
  - R4-002: `discard_stopped_jobs` continues the job in the background after the terminal was taken back.
  - R2-001/R2-002/R2-004: misleading `stopped_child_pending` name, raw close of OwnedFd gate pipe, `Terminal::new` side effects not obvious.
- [ ] **T4c — Job-control safety fixes from T4b review** (route: delegated → `codex-impl`; HIGH priority, before T6)
  - R4-kill-children-own-group / R3-kill-children-resolved-group: interactive `kill_children` resolves each child's pgid and SIGKILLs the whole group without checking it differs from the shell's own group -> cancellation/timeout can kill the shell itself (`eval.rs:971-985`).
  - R4-unbounded-ttin-spin / R3-ttin-spin-orphaned-group: `Terminal::acquire` busy-loops forever when started in an orphaned background group (SIGTTIN stop is discarded) (`terminal.rs:65-74`); bound it (sleep/backoff, orphan detection, fall back to no job control).
  - R3-terminal-modes-before-foreground: modes saved with tcgetattr before the shell is in the foreground (`terminal.rs:48-51`); save them after acquiring the terminal.
  - R2-dead-wuntraced-branch (`eval.rs:849-851`), polling interval constants (`eval.rs:939`): clean up.
- [ ] **T5 — Startup rc file** (route: delegated → `codex-impl`)
  - Source `$SO1SH_RC` or `~/.so1shrc` at startup when interactive; errors reported, not fatal.
  - Accept: PTY test with an rc that sets PS1 and an alias-free function.
- [ ] **T6 — Phase verification** (route: orchestrator + fresh Claude verifier subagent; Antigravity session closed by the user on 2026-10-05)
  - `make ci` + manual smoke script through a PTY; report.

## Verification evidence
- T1 (`57611af`): background job PIDs tracked per shell, cleared in forked children; `wait` waits only for those; `wait <pid>` returns its status, 127 if unknown. Tests 147/16/50/239/4 green (orchestrator spot-check, stdin=/dev/null); make ci only regression-37.
  Native review: high risk, user granted, approved, acknowledged. Advisory: EINTR retry makes `wait` non-interruptible by trapped signals (POSIX expects >128) -> Phase 2 traps work; operand validation and unknown-PID diagnostics are silent.

- T2 (`dc389bc` epsh: `epsh::cli` shared dispatch + `Shell::eval_repl_line`/`finish_repl`; `05d551a` so1sh crate): REPL with PS1, state persists, `exit n`/EOF statuses; 12 PTY + 5 CLI tests (rexpect 0.7.1, MIT/Apache-2.0, dev-only). Workspace 473 green (orchestrator spot-check).
  Native review: high risk, user granted, approved, acknowledged. WARNINGs -> T2b (errors end the REPL); SIGINT kills the shell -> T4. Suggestions: hardcoded prefix `cli.rs:19`, API surface addition not covered by api_stability.

- T1b (`24b23f7`): parser wrapped the whole accumulated sequential list in `Background` on `&` (`x=1; cmd &` ran `x=1` in a subshell); now `&` binds to the current AND-OR list only. Compared with dash (6 scripts). Workspace 480 green (orchestrator spot-check). `sys.rs` negative-PID mapping unreachable (crate-private, positive callers only) -> left unchanged.
  Native review: high risk, user granted, approved, acknowledged; 4 suggestions only.

- T2b (`91118c0`): REPL survives expansion errors, special-builtin errors (redirection, invalid break/continue operands, missing `.` file) per POSIX 2.8.1; only `exit`/errexit end the session; Cancelled -> 130, Stopped -> 148, both continue. Private `special_builtin_failed` marker; public `ShellError` unchanged. Workspace 497 green (orchestrator spot-check); orchestrator PTY probe: `local` scope is unwound after an error inside a function.
  Native review: high risk, user granted, approved, acknowledged. WARNINGs to revisit: sticky `special_builtin_failed` side channel (`eval.rs:289`), partial context restore in `eval_repl_line` (`eval.rs:906-913`), stopped child not adopted (`eval.rs:957-962`) -> Phase 2 job control.

- T3 (`c239849` epsh `Parser::parse_interactive`; `4c3d06b` so1sh reedline 0.52.1 MIT): PS1/PS2, persistent history (`$SO1SH_HISTFILE` or `~/.so1sh_history`), Ctrl-R, multiline via parser-backed validator, Ctrl-C clears line, Ctrl-D exits. epsh verified alone after its commit. Workspace 508 green (orchestrator spot-check). Limitation: reedline is Unicode-only (invalid PS1 bytes shown as U+FFFD).
  Native review: medium risk (slice budget), user granted, consolidated review approved, acknowledged. WARNINGs -> T3b.

- T4 (`28c3d89` epsh foreground job lifecycle + `discard_stopped_jobs`; `324d273` so1sh terminal ownership, ignored job-control signals at prompt, cooked modes restored): Ctrl-C 130, Ctrl-\\ 131, Ctrl-Z reports Stopped + discard, 148. Workspace 517 green (orchestrator spot-check); epsh verified alone. Codex disclosed initially invalid signal REDs (unflushed rexpect control bytes) and re-proved them against the baseline.
  Native review: high risk, user granted, 4 lenses approved, acknowledged. 8 WARNINGs -> T4b.

- T3b (`a601d55` epsh: forbidden line endings reported immediately, e.g. `echo >` like dash; `30a3fc9` shell: empty HOME/HISTFILE unset, Enter-newline validation for empty heredoc delimiter and backslash-newline, history I/O failure and exact Ctrl-R PTY tests). Workspace 527 green; epsh verified alone. History/Ctrl-R tests passed before the fix (coverage, disclosed by Codex).
  Native review: high risk, user granted, approved, acknowledged; 1 WARNING (`parser.rs:84`) + 3 suggestions.

- T4b (`9278d92` epsh: SIGINT/SIGQUIT abort the current list/loop with 130/131 incl. builtin loops; interactive waits honor cancel/timeout, signal pipeline pgids, reap members, join relays; stopped jobs discarded with SIGKILL; `8b0102e` shell: `Terminal::acquire` waits for foreground via SIGTTIN and restores the original owner; no controlling tty -> one diagnostic, REPL continues). Workspace 541 green; epsh verified alone. Orchestrator PTY probe (answering ESC[6n, script /tmp/claude-1000/so1sh-loop-probe.py): Ctrl-C aborts `sleep 100`, sleep loops and arithmetic loops -> PASS.
  Divergence from dash (orchestrator spec decision): Ctrl-\\ aborts the line with 131; dash continues the suffix after SIGQUIT.
  Codex observed one unexplained failure of `raw_bytes::path_lookup_handles_invalid_utf8_directories` during a concurrent run (rerun green, log kept) -> watch for flakiness.
  Native review: high risk, user granted, 4 lenses approved, acknowledged; 6 WARNINGs -> T4c (two are safety-critical).
  Lesson: PTY probes must answer reedline's cursor-position query (ESC[6n) or the editor blocks.

## Progress / Next step
- T5 running on `codex-impl`; then T4c (high priority), then T6 (orchestrator + Claude verifier).
- Branch `feat/phase-1-shell-mvp` (stacked on `chore/phase-0-workspace`); merge to `main` still pending the user's decision.

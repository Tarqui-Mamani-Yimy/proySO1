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
- [x] **T0c — Replace the sticky special-builtin failure flag** (codex-impl, with T1b)
  - Third review in a row flags `special_builtin_failed` as a stale global side channel (T0b review R3/R4: the `command` arm converts any Exit to MISUSE while the flag is stale, and never resets it before dispatch or after conversion, `builtins.rs:675-682`).
  - Redesign: carry the "recoverable special-builtin failure" cause in the error itself (private error variant or wrapper) instead of a shell-wide flag; remove the flag.
- [x] **T1b — Builtin hook follow-ups from review** (codex-impl, after T2)
  - R3-sink-vs-redirection: `write_stdout`/`write_stderr` prefer the sink even when the command's stdout is redirected (`hello > file` with a sink set writes to the sink, not the file); untested. Define and test the rule (redirection wins).
  - R4: a panicking embedder callback skips fd restoration (`eval.rs:1195`); add an unwind guard.
  - R3: functions now take precedence over regular epsh builtins (POSIX-correct, but a behavior change for scripts wrapping `cd`/`echo`); document it in AGENTS.md "Where we diverged"/changelog and add a test.
  - R3/R2: special-builtin names without an epsh implementation (e.g. `times`) now fail with CommandNotFound instead of falling back; decide and test; simplify the double dispatch.
- [x] **T2 — Job table and `jobs`** (codex-impl)
  - Ctrl-Z keeps the stopped job (no discard); `cmd &` registers a job; `jobs` lists `[n]+ Running/Stopped/Done  cmd`; `%n`, `%+`, `%-`, `%string` job specs.
- [x] **T2b — Job table follow-ups from review** (codex-impl, with T3/T4)
  - R3/R4-poll-reaps: the job poller reaps a background job's wait status before the script's `wait $!` can collect it, so `wait` returns 127 instead of the job's status (`eval.rs:1233-1237`); keep the status available to `wait` (POSIX: `wait pid` must return a known job's status).
  - R4-unreaped-job-stuck-running (`eval.rs:1211-1232`) and stopped-jobs entry not cleared on reap.
  - Job table never prunes Done/Killed jobs (`jobs.rs:43-74`): remove after they are reported (with T4 notifications).
  - Stopped jobs are never discarded on exit (`repl.rs:151`): handled by T4 (warn on exit, then HUP+CONT).
  - Dual stopped-job tracking in epsh (`eval.rs:1118`); timing-dependent PTY tests (`repl.rs:742-750`).
- [x] **T2c — Make `raw_bytes::path_lookup_handles_invalid_utf8_directories` hermetic** (codex-impl)
  - Rare flake seen in Phase 0 T2, Phase 1 T4b and Phase 2 (full `cargo test --workspace` run after `492aa1a`); 0/15 failures in isolated reruns before and after `492aa1a` -> pre-existing, not caused by T0c/T1b. Find the shared state (tmp names, PATH, cwd) and isolate it.
- [x] **T1c — Review correction: dup redirections must stay in sinks** (codex-impl; native review of `492aa1a` returned correction_required, plan 80 lines)
  - R3-dup-redirect-escapes-sinks (CRITICAL, `eval.rs:908-910`): with sinks set, `echo msg >&2` / `cmd 2>&1` write to the real process fd and escape the embedder's capture. File redirections bypass the sink; dups between stdout/stderr route to the matching sink.
- [x] **T1d — Sink-route follow-ups from the `49243c1` review** (codex-impl, after T2d)
  - R3-route-ignores-substitution-capture: the shared stdout route atomic stays at "real fd" inside compound bodies, so fork-free command substitution in `{ v=$(echo hi); } >file` (or `>&2`) may send `echo` to the file instead of the capture (`eval.rs:914-919`). Verify and fix: substitution capture must override the route.
  - R4-compound-redirect-panic-leaks-route: brace groups, EV_EXIT subshells and function-definition redirections restore by hand, without the RAII guard; a panicking embedder builtin there leaks the route (`eval.rs:1855`).
  - R2: route atomics readability (`eval.rs:374-377`, `redirect.rs:60-69`); failed dup routes to real fd (`redirect.rs:288-295`).
- [x] **T2d — Flaky PTY test `pipeline_ctrl_z_is_retained_and_cleaned_up_by_test`** (codex-impl)
  - Failed 4 of 11 full workspace runs in Codex's T2c verification (`crates/shell/tests/repl.rs:636`). Find the race and make it deterministic.
- [ ] **T2e — `make ci` hang with a terminal stdin** (investigate)
  - Codex reported `make ci` (run without `< /dev/null`, from its interactive terminal) hanging ~5 min in the workspace test phase; orchestrator could not reproduce with stdin=/dev/null (3/3 workspace runs ~6 s, make ci 7 s). Hypothesis: a test inherits a terminal stdin and blocks. Reproduce under a PTY and make the suite independent of the caller's stdin.
- [x] **T3 — `fg` and `bg`** (codex-impl)
  - `fg %n` gives the job the terminal, SIGCONT to the group, waits, restores modes; `bg %n` resumes in background; status codes per POSIX.
  - Fix: SIGTTIN acquisition deadline must not count time spent stopped (`terminal.rs:74-79`).
- [ ] **T3b — fg/bg/wait follow-ups from the consolidated review** (codex-impl, after T4)
  - `wait %n` numbers jobs from epsh's append-only `owned_job_history`, while `jobs`/`fg`/`bg` use the shell's JobTable (pruned/renumbered) -> `%n` can name different jobs (`builtins.rs:777-786`); a single source of truth for job specs.
  - `wait %n` with a cached status leaves the pid in `background_pids`; a later bare `wait` waits on a reaped/reused pid.
  - `reaped_job_statuses` grows without bound and can return a stale status for a recycled pid (`eval.rs:1249-1255`).
  - `fg` ignores tcsetpgrp/tcsetattr failures and does not reject Done/Killed jobs (`jobs.rs:301-303`).
  - New PTY test races `fg`'s tcsetpgrp (`repl.rs:1392-1393`): synchronize like T2d.
- [x] **T4 — Job notifications** (codex-impl)
  - Before each prompt report `[n]+ Done/Stopped/Killed` for changed background jobs; `exit` with stopped jobs warns once (like bash).
- [ ] **T4b — Job notification follow-ups from review** (codex-impl, after T3b)
  - R3-launch-notice-uses-redirected-stdout: the `[n] pgid` launch notice is written through the current stdout route, so `{ sleep 1 & } > file` puts it into the file (`eval.rs:1253-1259`); job notices belong on the terminal (bash writes them to stderr/tty).
  - R3-fg-restop-notification-unproved: a job resumed with `fg` and stopped again must print `[n]+  Stopped` (`jobs.rs:321`); add the PTY test.
  - Suggestion: test that the stopped-jobs exit warning resets after a non-exit command (`eval.rs:1730`).
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

- T2 (`252bbac` epsh owned-job info/state API polled per job pid; `0ce2047` shell job table + `jobs` builtin, Ctrl-Z keeps jobs, `[n] pgid` on `&`; AGENTS.md updated by orchestrator). Workspace 579 green; epsh verified alone. Codex's session restarted mid-report (user reopened it); its report matched the orchestrator's verification.
  Native review: high risk, user granted, approved, acknowledged; 8 WARNINGs -> T2b.

- T0c+T1b (`492aa1a`, one unit: changes interleave in eval.rs): shell-wide `special_builtin_failed` removed, replaced by internal `EvalError::RecoverableSpecialBuiltin`; embedder builtin output follows redirected fd 1/2 instead of the sink; RAII fd guard on callback panic; `times` implemented; function-over-builtin precedence documented. Orchestrator found 2 tests flaky under the parallel runner (fd-1 redirection test, process-group test) that Codex had reported green -> Codex isolated them; orchestrator verified 10/10 parallel runs green.
  Process note: the orchestrator committed in the same command as a workspace run that showed the rare `path_lookup_handles_invalid_utf8_directories` flake; checked afterwards: 0/15 failures on both `492aa1a` and `0ce2047` -> pre-existing (T2c).

- T2c: Codex could not reproduce the rare flake; removed the last shared dependency (copied Cargo `show_argv` fixture) by creating a private script in the test's temp PATH dir; integration 20/20 green. Committed by orchestrator; `.codegraph/` added to `.gitignore`.
- T0c/T1b native review (`492aa1a`): `managed_assets_outdated` stop -> ran the provider's `gentle-ai sync --agent claude-code` (updated ~/.claude/CLAUDE.md; user-modified review agents preserved); then correction_required (R3-dup-redirect-escapes-sinks) -> plan 80 lines -> T1c.

- T1c (`49243c1`): per-fd sink provenance copied on dup and restored in reverse order; file redirections bypass the sink, `>&2`/`2>&1`/`1>&2` route to the matching sink. Diff 133+/14- exceeded the declared 80-line correction budget without notice (flagged to Codex). Orchestrator verified 5/5 embedding runs + workspace green.
  Native review: the correction changed scope -> provider-issued `review recover` (scope_changed, new lineage review-802bc3456c12c1a9) -> 4 lenses approved, acknowledged. Covers `492aa1a`..`49243c1`. WARNINGs -> T1d.

- `808fac5` chore: `.atl/` untracked (user approved) — tool cache kept re-triggering review consent.
- T2d (`fd15f4b`): pipeline Ctrl-Z test assumed a returned prompt meant all killed members were reaped; now waits for pgid membership and observable Killed/Done states, no fixed sleeps. Orchestrator: 5/5 full workspace runs with 0 failures.
- T1d (`eb83a98`): confirmed real bug — `{ v=$(echo hi); echo "[$v]"; } > out` wrote `hi\n[]` to the file; fork-free substitution now overrides the sink route; RedirectionGuard on compound/EV_EXIT/function paths; named route states; failed dup keeps errno. Orchestrator: 3/3 workspace runs green (~6 s), make ci only regression-37.

- T2b+T3 (`57783ec` epsh: reaped statuses kept for `wait`, stuck-running fix; `01e60a5` shell: `fg`/`bg`, Done/Killed pruned after `jobs`, SIGTTIN deadline excludes stopped time). Orchestrator: 5/5 workspace runs, make ci, PTY probe (Ctrl-Z, jobs, bg, fg+Ctrl-C=130, `sh -c "exit 7" & wait $!`=7). Codex confirmed the earlier make ci hang came from running it with a terminal stdin (T2e).
  Native consolidated review (`49243c1..01e60a5`, medium, slice budget): user granted, approved, acknowledged; 5 WARNINGs -> T3b.

- T4 (`541d37b` epsh: immediate launch callbacks, opt-out of the stopped diagnostic; `79fcedc` shell: prompt-time Done/Exit/Killed/Stopped notifications with pruning, two-step exit with stopped jobs + SIGHUP/SIGCONT, Ctrl-Z prints only the job line). 8 existing PTY tests updated to the new behavior without weakening (per-test report from Codex). Orchestrator: 3/3 workspace runs, epsh alone green.
  Native consolidated review (medium): user granted, approved, acknowledged; 2 WARNINGs -> T4b.

## Progress / Next step
- T3b running on `codex-impl`; then T4b, T2e, T5, T6, T7, T8.

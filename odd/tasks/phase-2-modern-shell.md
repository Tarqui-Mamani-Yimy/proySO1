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
- [x] **T2e — `make ci` hang with a terminal stdin** (investigate) — hang not reproduced; real defect fixed in `8794c70`: isolated test children could open the caller's /dev/tty despite null stdin -> `setsid`; 3/3 runs with PTY stdin, 3/3 with /dev/null. `b073d39` reformatted a848e3f (orchestrator committed it without rustfmt; make ci fmt check had failed).
  - Codex reported `make ci` (run without `< /dev/null`, from its interactive terminal) hanging ~5 min in the workspace test phase; orchestrator could not reproduce with stdin=/dev/null (3/3 workspace runs ~6 s, make ci 7 s). Hypothesis: a test inherits a terminal stdin and blocks. Reproduce under a PTY and make the suite independent of the caller's stdin.
- [x] **T3 — `fg` and `bg`** (codex-impl)
  - `fg %n` gives the job the terminal, SIGCONT to the group, waits, restores modes; `bg %n` resumes in background; status codes per POSIX.
  - Fix: SIGTTIN acquisition deadline must not count time spent stopped (`terminal.rs:74-79`).
- [x] **T3b — fg/bg/wait follow-ups from the consolidated review** (codex-impl, after T4)
  - `wait %n` numbers jobs from epsh's append-only `owned_job_history`, while `jobs`/`fg`/`bg` use the shell's JobTable (pruned/renumbered) -> `%n` can name different jobs (`builtins.rs:777-786`); a single source of truth for job specs.
  - `wait %n` with a cached status leaves the pid in `background_pids`; a later bare `wait` waits on a reaped/reused pid.
  - `reaped_job_statuses` grows without bound and can return a stale status for a recycled pid (`eval.rs:1249-1255`).
  - `fg` ignores tcsetpgrp/tcsetattr failures and does not reject Done/Killed jobs (`jobs.rs:301-303`).
  - New PTY test races `fg`'s tcsetpgrp (`repl.rs:1392-1393`): synchronize like T2d.
- [x] **T4 — Job notifications** (codex-impl)
  - Before each prompt report `[n]+ Done/Stopped/Killed` for changed background jobs; `exit` with stopped jobs warns once (like bash).
- [x] **T4b — Job notification follow-ups from review**
- [x] **T3c — wait/notification follow-ups from the T3b review** (codex-impl, next)
  - R3-notification-discards-wait-pid-status: prompt-time notifications discard the cached reaped status while the pid stays in `background_pids`, so `sleep 0.1 &` ... (Done notified) ... `wait $!` returns 127 instead of the status (`jobs.rs:541`). bash still returns the status; match it.
  - R3-wait-failure-records-fake-exit-zero: a non-EINTR waitpid failure yields raw status 0 and records a fake `Exited(0)` for an owned pid (`builtins.rs:837-839`).
  - R3-fg-completed-test-weak-assertions: the fg-rejection test matches any `fg:` error and `1\n` can match a pgid (`repl.rs:814-816`); assert the exact message and status. (codex-impl, after T3b)
  - R3-launch-notice-uses-redirected-stdout: the `[n] pgid` launch notice is written through the current stdout route, so `{ sleep 1 & } > file` puts it into the file (`eval.rs:1253-1259`); job notices belong on the terminal (bash writes them to stderr/tty).
  - R3-fg-restop-notification-unproved: a job resumed with `fg` and stopped again must print `[n]+  Stopped` (`jobs.rs:321`); add the PTY test.
  - Suggestion: test that the stopped-jobs exit warning resets after a non-exit command (`eval.rs:1730`).
- [x] **T2f — Systematic PTY test reliability** (codex-impl)
  - PTY tests time out intermittently under parallel load (Codex T3c run: `foreground_ctrl_z_is_reported_and_cleaned_up_by_test` 1/5; orchestrator T4b runs under review load). Serialize PTY tests in the repl binary, centralize generous timeouts (env-overridable), remove remaining fixed sleeps; prove with 10 workspace runs under an artificial CPU hog.
- [x] **T5 — Shell integration marks** (codex-impl)
  - Emit OSC 133 A/B/C/D (prompt start, command start, output start, command end with exit code) and OSC 7 (cwd as file:// URL) when interactive; opt-out variable.
  - Accept: PTY tests assert exact sequences and ordering.
- [x] **T6 — Tab completion** (codex-impl) — `77c88ce` OSC 7 hostname, `8115caf` completion.
- [x] **T6b — Review correction (plan 40 lines)** — `a848e3f`; root cause: PTY helper uses TERM=dumb and reedline's plain menu uppercases the selected candidate.: native review lineage review-065e210f56cc033f returned correction_required for `tab_lists_multiple_file_matches` (`crates/shell/tests/repl.rs:2318`) expecting `MULTI-FIRST.TXT`; real terminal output is lowercase (orchestrator probe) yet the test passes -> it does not prove what it claims. Fix the expectation, explain why uppercase matched, commit, then re-query STATUS for that lineage (expect corrected_candidate -> targeted validation -> acknowledge).
  - Commands (builtins, functions, embedder builtins, PATH executables), file paths (quoting spaces), `$VAR` names.
- [x] **T7 — Hints and syntax highlighting** (codex-impl)
  - History-based ghost-text hints; highlighting via the epsh lexer (commands found/not found, strings, operators).
- [x] **T6c — Completion follow-ups from the T5-T7 review** (codex-impl) — `354ff74` symlinked/slash-path executables, `0a2c4ba` integration write failures no longer end the REPL (committed by the user).
  - R3-path-symlink-executables-dropped: `executable_names` uses `DirEntry::metadata`, which does not follow symlinks, so symlinked executables in PATH (python3, alternatives, Homebrew/Nix/asdf shims) never complete and — through the shared cache — are highlighted as unknown (`completion.rs:165-168`).
  - R3-path-prefixed-command-completion-empty: a command-position prefix containing `/` (`./scr<Tab>`, `/usr/bin/py<Tab>`) offers nothing; it must fall back to executable file completion (`completion.rs:132-136`).
  - Suggestions: highlighter byte/char offset unit test (`highlighter.rs:32-37`); a terminal write failure of integration marks should not end the REPL (`repl.rs:230-231`); per-prompt PATH scan cost (`repl.rs:225`).
- [x] **T8 — Phase verification** — user stopped the Claude verifier and assigned it to Codex (note: not independent, Codex wrote the phase). Workspace 621 tests green twice; make ci only regression-37; PTY smoke a,b,c,d,f,h PASS. (e) OSC 7 `localhost`: false alarm, this machine's hostname is `localhost` (orchestrator checked hostname/uname -n/gethostname). (g) real defect -> T8b.
- [x] **T8b — `SO1SH_HIGHLIGHT=0` still emits SGR** (codex-impl) — `9067920` PlainHighlighter; PTY RED showed `\x1b[37m`, GREEN after.: reedline falls back to its default highlighter (`\x1b[37m`) when ours is skipped (`repl.rs:207-212`); install a plain no-style highlighter.

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

- T3b (`076053c` epsh: interactive `wait %n` resolves through the frontend job table, cached-status waits clear background pids, bounded reaped-status cache; `e64f35b` shell: fg rejects Done/Killed jobs, checks terminal handoff, fg PTY test synchronized). Orchestrator 3/3 workspace runs. Native consolidated review approved; 3 WARNINGs -> T3c.
- T4b (`f377798`): job notices use the stderr route; RED proved `{ sleep 1 & } > file` wrote the notice into the file (bash shows it on the terminal). Orchestrator: 2 of 3 workspace runs failed while the native review was running in parallel (machine under load), then 4/4 clean runs -> PTY timing under load to watch.

- T3c (`1533745` epsh: no fabricated waitpid statuses — EINTR retry, ECHILD not-found, other errors failure; `e0ea0ac` shell: notifications keep the cached status so `wait $!` after `Done` returns the real status, verified against bash PTY = 7; exact fg-rejection assertion). Codex honestly reported a parallel PTY flake (1/5) -> T2f.

- T2f (`58076a7`): process-wide mutex serializes the 78 PTY tests, one 20 s timeout (`SO1SH_TEST_TIMEOUT_SECS`), state-synchronized polling. Codex 10/10 and orchestrator 3/3 workspace runs green with CPU hogs; repl binary ~6 s -> ~50 s (accepted tradeoff).
  Native consolidated review of `f377798..58076a7` (T4b, T3c, T2f): medium, user granted, approved with suggestions only (no WARNINGs), acknowledged. Suggestion: `yield_now` spin polling under load (`repl.rs:1112`).

- T5 (`9b8b256`): OSC 7 before each prompt + OSC 133 A/B/C/D (ST terminator), terminal-only, one block for multiline, none for Ctrl-C-discarded input, `SO1SH_SHELL_INTEGRATION=0` opt-out; reedline secondary-prompt callbacks no longer consume B early; 12 new PTY tests incl. wrapped long input; legacy PTY fixtures opt out of marks. Orchestrator PTY probe: `false` -> `133;D;1`, `cd /tmp` -> `7;file://localhost/tmp`.
  Follow-up: OSC 7 uses host `localhost`; terminals use the real hostname to detect remote sessions -> use gethostname.


## Pause point (2026-10-06, usage limit reached)
- Resumed 2026-10-07: T7 finished and committed; a rustc nightly ICE report (crash while formatting a closure type error in WIP code) appeared and is now gitignored.
- Pending: T6b correction (above), T7, T2e, T8. Then Phase 2 closes.

- T7 (`38d6e97`): history ghost-text hints (Right/End accept), epsh-lexer highlighting (resolved vs unknown commands via per-prompt PATH cache, strings, operators, variables, comments), never alters submitted text, plain-text fallback on lexer errors, `SO1SH_HIGHLIGHT=0` opt-out; dependency nu-ansi-term (MIT, already in reedline's tree). Orchestrator 2/2 workspace runs green.

- Native review for T5..T7+T6b: correction -> `a848e3f` -> provider `recover` (scope_changed, lineage review-bd6c08d86940dc7f) -> consolidated review approved, acknowledged; 2 WARNINGs -> T6c.


## Phase 2 result
- All implementation and verification tasks done on `feat/phase-2-modern-shell` (stacked on Phase 1). Last commit `9067920`.
- Native review: everything up to `a848e3f` reviewed and acknowledged. Pending unreviewed slice `a848e3f..9067920` (T2e test isolation, rustfmt, T6c, T8b) is under the slice budget, so the native assessment did not require review yet.
- Not pushed or merged: delivery (merges to `main`, PR/chain strategy) is the user's decision.

## Progress / Next step
- Next: T6 (tab completion) on `codex-impl` (+ OSC 7 hostname); then T7, T2e, T8.

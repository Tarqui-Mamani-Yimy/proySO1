# epsh — trap timing during waits (sugPerms.md item 5)

Objective: run traps with dash/POSIX timing, including during `wait` and builtin-only loops, using sigaction and a self-pipe (no signal-hook).
Branch: `fix/trap-timing`. Delivery: merge to main on the user's request. Test runner: `cargo test -p epsh --test integration traps`.

## Specs

- **S0** — sugPerms.md item 5 (verbatim): "**Traps durante esperas bloqueantes** (`TODO.md` §4). Esfuerzo: medio, código de señales delicado." / "Usar `sigaction` sin `SA_RESTART` o un self-pipe, sin sumar `signal-hook` (`signal.rs:8-10` explica por qué se evita el trampolín de rustix)." / "Tests primero."
- **S1** — A pending trap runs as soon as the current command completes, before the next command (dash order: `trap "echo T" TERM; (sleep 0.3; kill -TERM $$) & sleep 1; echo after` prints `T` then `after`), also inside loops made only of builtins. `$?` after the trap action is the value before it; `exit` inside the trap action ends the shell with that status.
- **S2** — Every trappable signal runs its trap (USR1, USR2, ALRM, QUIT, ...), not only INT, TERM and HUP.
- **S3** — The `wait` builtin returns immediately with 128+signal when a signal with a trap arrives, then the trap runs (dash: `wait` → `T st=143`); interrupted jobs stay registered and can be waited for later. Wake-up is race-free via a self-pipe.
- **S4** — Forked subshells reset caught (non-ignored) traps to the default action, as POSIX and dash do (dash: a TERM sent to a subshell after `trap ... TERM` in the parent kills it with 143).
- **S5** — A foreground external command, pipeline or command substitution is not interrupted: its trap runs right after it completes (POSIX "shall not be executed until after the foreground command has completed").
- **S6** — Docs: AGENTS.md Known Limitations and TODO.md §4 reflect the new model.

## Tasks

| ID | Specs | Route | Status | Commit |
|---|---|---|---|---|
| T1 — per-command trap dispatch, all signals, subshell reset | S1, S2, S4, S5 | inline (no writer reason) | done | see L5 |
| T2 — interruptible `wait` via self-pipe | S3 | inline | done | see L5 |
| T3 — docs | S6 | inline | done | see L5 |

## Log

- **L1** (2026-11-21, user): "si, mergealo y vamos con la siguiente" (next = sugPerms.md item 5).
- **L2** baseline (dash vs epsh, `-c`): epsh prints `after T` (trap one top-level command late); USR1 trap never runs; `wait` returns 0 after the job ends instead of 143; `while :; do :; done` never runs a TERM trap; subshells keep the parent's caught traps. Risk: item 4 (signal/async ordering) → independent verifier at the end.
- **L3**: RED observed: 8 of 11 new `traps` tests failed on base (3 pinned already-correct behavior); every expectation validated against dash. GREEN: 11/11, three runs, and under full CPU load.
- **L4**: Independent verifier (Risk item 4): no blockers, S1–S6 PASS. Its probes became `traps_verification_probes_match_dash`; one (`kill -USR1 $$ & wait $!`) dropped as timing-dependent (signal may land before `wait` starts; both outcomes are POSIX-valid), one expectation corrected (`"[$x]"` keeps the newline). Advisory notes kept as follow-ups: `trap '' CHLD` breaks foreground commands (pre-existing, "No child processes"); single global WAKE_FD for multithreaded embedders; embedder SIGCHLD handler replaced during `wait`; numeric trap specs install no handler; Ctrl-C during `wait` in so1sh not checked in the loop (pre-existing). BSD errno added (netbsd/openbsd).
- **L5**: Checks: `cargo test --workspace` all green (integration 266 in 1.08 s); clippy clean; mksh conformance 167 pass / 1 fail (`regression-37`, identical on base).
- **L6** (2026-11-22, user granted native review): lineage `review-fa4011d8060e60b5` on 86653ee, lenses resilience + reliability → approved, acknowledged (authority burned). Advisory findings: `in_trap` survived fork (fixed in the next commit with a RED/GREEN test), `wait %N` and background/pipeline reset untested (tests added, already correct), SIGCHLD replaced during `wait` for embedders with their own reaper (kept as follow-up, see L4).

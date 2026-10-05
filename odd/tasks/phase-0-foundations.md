# Phase 0 — Foundations

## Objective
Turn the imported epsh code into a versioned, buildable Cargo workspace with a
green baseline, ready to host the `shell` and `terminal` crates (planing.md §4–§5).

## Problem / Why
The repository had no commits, an unverified toolchain, and a single-crate layout.
Every later phase needs a trustworthy baseline (all tests green) and a workspace
to add crates without touching the engine.

## Scope
- In: git baseline, toolchain verification, workspace conversion (`crates/epsh`),
  path updates in Makefile/Dockerfile/docs/scripts, a local CI entry point.
- Out: any behavior change in epsh, new crates (`shell`, `terminal`), CI hosting.

## Constraints
- No behavior change: the existing test suites are the oracle.
- epsh stays publishable as the `epsh` crate (same name, same public API).
- Single writer: `codex-impl`. Verifier: `agy-research` (read-only).
- Work on branch `chore/phase-0-workspace`; never commit to `master` except T1.

## TDD
- Mode: strict (source: user global config "Strict TDD Mode: enabled").
- Runner: `cargo test` (+ `perl check.pl -p ./target/debug/epsh -s check-epsh.t`).
- T3/T4 are behavior-preserving refactors: the RED step is replaced by
  "baseline green before, identical results after"; any new script gets a check.

## Delivery
- Strategy: `ask-on-risk` (default). Forecast: mostly moves; authored lines < 400.

## Tasks

- [x] **T1 — Baseline commit** (route: inline, orchestrator; state-only git op)
  - Add `.gitignore` (`/target`, `.atl/`), commit current tree on `main`.
  - Accept: `git log` shows one root commit; `git status` clean.
- [x] **T2 — Baseline verification** (route: delegated → `agy-research`)
  - Run `cargo test`, `cargo clippy --all-targets`, build + `check.pl` on the root commit.
  - Accept: exact pass/fail counts recorded below; known failures listed.
- [x] **T3 — Workspace conversion** (route: delegated → `codex-impl`; writer trigger: 2+ files)
  - Root `Cargo.toml` becomes a virtual workspace; epsh moves to `crates/epsh`
    (src, tests, fixtures, Cargo.toml). `Cargo.lock`, `rust-toolchain.toml`, target dir stay at root.
  - Update paths in Makefile, Dockerfile, AGENTS.md, README, tests/stress, check.pl invocations.
  - Accept: same test results as T2; `./target/debug/epsh` still produced.
- [x] **T4 — Local CI entry point** (route: delegated → `codex-impl`)
  - `make ci` runs fmt check, clippy (deny warnings), `cargo test --workspace`, conformance.
  - Accept: `make ci` exits 0 (or fails only on T2-recorded pre-existing issues).
- [x] **T5 — Independent verification** (route: delegated → `agy-research`)
  - Re-run T2 suite on the T3/T4 commits; review diff read-only.
- [x] **T6 — Fix deterministic baseline failures and make tests hermetic** (route: delegated → `codex-impl`; approved by user 2026-10-05)
  - Fix `exec_redirects::exec_read_write` (`read line <&4` reads stdin instead of fd 4) — real redirection bug.
  - Fix `raw_bytes::cd_into_invalid_utf8_directory_and_use_relative_path` and `raw_bytes::glob_matches_invalid_utf8_filenames` (FromUtf8Error on non-UTF-8 paths): decide whether the bug is in epsh or in the test, with evidence.
  - Make the suite hermetic: results must not depend on the caller's stdin or on shared `/tmp` names (exec_close_fd, path_lookup_*, output_sinks::*).
  - Accept: `cargo test --workspace` fully green, identical for codex-impl and agy-research environments; strict TDD (RED observed first).
  - Out of scope: `regression-37` (`time -p`) is environmental — POSIX `time` is an external utility and `/usr/bin/time` is not installed here.

## Verification evidence
- T1: root commit `da10eb0` "begin" (by the user, includes `.atl/`), then `7b66dc1` adds `.gitignore` + this document. `git status` clean after commit. Note: `.atl/` is already tracked, so the ignore rule does not untrack it.

- T2 (agy-research, commit `7b66dc1`, report /tmp/so1shell-t2-report.md):
  - `cargo build`: OK (5 manifest warnings).
  - `cargo test`: lib 147 ok, api_stability 16 ok, embedding 43 ok, integration 223 ok / **3 failed**:
    `exec_redirects::exec_read_write`, `raw_bytes::cd_into_invalid_utf8_directory_and_use_relative_path`,
    `raw_bytes::glob_matches_invalid_utf8_filenames`; `raw_bytes::path_lookup_handles_invalid_utf8_directories` flaky in parallel.
  - `cargo clippy --all-targets`: 0 errors; 4x non_kebab_case_bins, 1x manual_readme, 1x useless_conversion (`src/sys.rs:147`).
  - `check.pl`: 167/168, failing `regression-37` (`time -p`, not implemented).
  - These are **pre-existing baseline failures**; out of Phase 0 scope (no behavior change). Candidate backlog for a later fix task.
- T3 (codex-impl, commit `6d087a6` on `chore/phase-0-workspace`): 41 files, +107/-80, all Rust sources 100% renames.
  build OK; tests lib 147 / api 16 / embedding 43 ok; integration 223 ok / 3 failed (set differs from T2:
  `exec_redirects::exec_close_fd` appeared; Codex also saw `output_sinks::separate_stdout_stderr` once) -> suspected flakiness, T5 checks it.
  clippy OK; check.pl 167/168 (regression-37); fmt check OK; doc tests 4 ok.
  Native review: risk high, user granted, 4 lenses, **approved**, acknowledged (lineage review-d6d5459331605811).
  Advisory (non-blocking) findings to fold into T4: dead `exclude` entries `crates/epsh/Cargo.toml:11`;
  duplicated LICENSE `crates/epsh/LICENSE`; magic relative target path `crates/epsh/tests/stress/run.sh:6`;
  test cwd shift after move (`crates/epsh/Cargo.toml`).
- T5 (agy-research, 5 runs on `6d087a6`): 3 deterministic failures (exec_read_write, 2x raw_bytes invalid UTF-8); exec_close_fd / separate_stdout_stderr / path_lookup passed 5/5. Codex's environment shows the opposite for exec_read_write vs exec_close_fd -> tests depend on caller environment (likely inherited stdin).
- T4 (codex-impl, `b23afaa` + review correction `543b413`): `make ci` (fmt check, clippy -D warnings, build, test, check.pl; fail-fast); stress/run.sh resolves workspace target dir; portable bump-version sed; `scripts/test-local-ci.sh` checks; LICENSE kept (needed by `cargo package`).
  clippy -D warnings PASS; tests same 3 failures; check.pl 167/168.
  Native review: risk high, user granted; correction required (CRITICAL R3/R4: `.into()` removal broke u16 mode_t on macOS/BSD) -> fixed in `543b413` with `u32::from` + scoped allow; targeted validation approved; acknowledged (lineage review-f01800a753e87178). Darwin cross-check not run (target not installed).
  Advisory: `make ci` duplicates test step (Makefile:13-15).
- T6 (codex-impl writes; orchestrator commits because `~/.codex/rules/orquestador-deny.rules` forbids git add/commit for Codex):
  - `d4cf178` fix: redirection fd closed when open() returned the target fd (dup2 no-op + drop). Review approved.
  - `93f0f85` test: invalid-UTF-8 tests wrongly decoded stdout; epsh was correct. Review approved.
  - `63e7aaa` test: hermetic tests (child-process isolation for fixed-fd / waitpid(-1) tests, private PATH, null stdin, tempdirs). Review approved.
  - `984c457` test: reject vacuous `--exact` child runs (review WARNINGs on `63e7aaa`). Review approved.
  - Verification: codex-impl, orchestrator and agy-research (`/tmp/so1shell-t6-verify.md`) all 442/442 green (parallel, serial, stdin=/dev/null).
    `make ci`: fmt, clippy -D warnings, build, tests PASS; check.pl 167/168 (regression-37, `/usr/bin/time` absent).
  - Engine bug found, fixed in Phase 1 T1 (`57611af`): `wait` builtin calls `waitpid(-1)` (`crates/epsh/src/builtins.rs:723`) and can reap the embedder's children.
  - Open advisory suggestions (non-blocking): fcntl error handling note in `redirect.rs:22-27`, test-helper duplication, comments in embedding.rs.

## Phase 0 result
- All tasks done on `chore/phase-0-workspace` (9 commits over `main`). Not pushed or merged: delivery is the user's decision.

## Progress / Next step
- Next: user decides merge of `chore/phase-0-workspace` into `main`; then Phase 1 (shell MVP) feature document.

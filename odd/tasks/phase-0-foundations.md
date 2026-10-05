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
- [ ] **T4 — Local CI entry point** (route: delegated → `codex-impl`)
  - `make ci` runs fmt check, clippy (deny warnings), `cargo test --workspace`, conformance.
  - Accept: `make ci` exits 0 (or fails only on T2-recorded pre-existing issues).
- [ ] **T5 — Independent verification** (route: delegated → `agy-research`)
  - Re-run T2 suite on the T3/T4 commits; review diff read-only.

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

## Progress / Next step
- Next: T5 (flakiness characterization) running on `agy-research`; then T4 (`make ci` + advisory cleanups).

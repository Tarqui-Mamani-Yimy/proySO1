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

- [ ] **T1 — Baseline commit** (route: inline, orchestrator; state-only git op)
  - Add `.gitignore` (`/target`, `.atl/`), commit current tree on `master`.
  - Accept: `git log` shows one root commit; `git status` clean.
- [ ] **T2 — Baseline verification** (route: delegated → `agy-research`)
  - Run `cargo test`, `cargo clippy --all-targets`, build + `check.pl` on the root commit.
  - Accept: exact pass/fail counts recorded below; known failures listed.
- [ ] **T3 — Workspace conversion** (route: delegated → `codex-impl`; writer trigger: 2+ files)
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
_(filled per task)_

## Progress / Next step
- Next: T1.

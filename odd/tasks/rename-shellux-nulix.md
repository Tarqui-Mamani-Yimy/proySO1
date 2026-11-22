# Rename so1sh → Shellux and so1term → Nulix (before packaging, sugPerms.md item 8)

Objective: give the shell and the terminal their product names everywhere a user or developer sees them, with no compatibility aliases (nothing has been released yet).
Branch: `refactor/rename-shellux-nulix`. Delivery: merge to main on the user's request. Test runner: `cargo test --workspace` plus `make ci`.

## Specs

- **S1** — User request (verbatim): "la shell se llamara Shellux y la terminal se llamara Nulix, esto delegaselo a agy, via HerdrCLI".
- **S2** — Scope chosen by the user (verbatim option): "Renombre completo (Recommended)" — "Binarios shellux, nulix, nulix-server, nulixctl; crates; variables SHELLUX_* y NULIX_* (antes SO1SH_*/SO1TERM_*); rutas ~/.shelluxrc, ~/.shellux_history y ~/.config/nulix/nulix.conf; sockets, docs y tests. Sin alias de compatibilidad, porque todavía no hay ningún release publicado. epsh no cambia."
- **S3** — Name mapping (derived from S2):
  - Binaries: `so1sh` → `shellux`; `so1term` → `nulix`; `so1term-server` → `nulix-server`; `so1ctl` → `nulixctl`.
  - Crates/packages: `so1sh` → `shellux`; `so1-terminal` → `nulix` (lib crate name `nulix`); `so1-term-core` → `nulix-core` (lib `nulix_core`). Crate directories under `crates/` may keep their current names (`shell`, `terminal`, `term-core`).
  - Environment variables: every `SO1SH_*` → `SHELLUX_*` (e.g. `SO1SH_RC` → `SHELLUX_RC`, `SO1SH_HISTFILE`, `SO1SH_POLICY`, `SO1SH_BANNER`, `SO1SH_HIGHLIGHT`, `SO1SH_SHELL_INTEGRATION`, `SO1SH_KITTY_KEYBOARD`); every `SO1TERM_*` → `NULIX_*` (e.g. `SO1TERM_CONFIG`, `SO1TERM_SHELL`, `SO1TERM_SOCKET`, `SO1TERM_CONTROL`, `SO1TERM_FONT`, `SO1TERM_FONT_SIZE`, `SO1TERM_GRAPHICS_ALLOWED_ROOTS`).
  - Paths: `~/.so1shrc` → `~/.shelluxrc`; `~/.so1sh_history` → `~/.shellux_history`; `~/.config/so1term/so1term.conf` → `~/.config/nulix/nulix.conf`; `docs/so1term.conf.example` → `docs/nulix.conf.example`; runtime sockets `so1term/so1term-<pid>.sock` → `nulix/nulix-<pid>.sock`, `so1term-server-<NAME>.sock` → `nulix-server-<NAME>.sock`, temp dir `so1term-<uid>` → `nulix-<uid>`.
  - User-visible text: diagnostics prefixes (`so1sh:` → `shellux:`, `so1term:` → `nulix:`, `so1term-server:` → `nulix-server:`, `so1ctl:` → `nulixctl:`), `--help`/usage text, banner "shell" field, tab label fallback (`so1sh` → `shellux`), window title, GPU device labels.
- **S4** — Unchanged: `epsh` (crate, binary, API, `EPSH_*` if any); the repository/workspace name `so1Shell`; historical records under `odd/tasks/` other than this file (they describe past work); `sugerencias.md`/`resumen.md` (local, untracked); git history.
- **S5** — No compatibility aliases or fallbacks for old names. A leftover `so1sh`/`so1term`/`so1ctl`/`SO1SH_`/`SO1TERM_` string outside S4 is a defect; check with `grep -rniE 'so1sh|so1term|so1ctl|so1_term|so1-term' --exclude-dir=target --exclude-dir=.git --exclude-dir=odd .`.
- **S6** — Checks: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets` (no new warnings), `cargo test --workspace` green, `perl check.pl -p ./target/debug/epsh -s check-epsh.t` unchanged (167 passed, 1 failed: regression-37). Docs (AGENTS.md, README.md, TODO.md, docs/*) updated together with code.

## Tasks

| ID | Specs | Route | Status | Commit |
|---|---|---|---|---|
| T1 — full rename, code + tests + docs | S1–S6 | delegated to agy via Herdr (user request) | done | see L4 |

## Log

- **L1** (2026-11-22, user): "la shell se llamara Shellux y la terminal se llamara Nulix, esto delegaselo a agy, via HerdrCLI".
- **L2** (2026-11-22, user answer): scope "Renombre completo (Recommended)" (S2).
- **L3** (2026-11-22, agy): Completed T1 full rename across crates (shellux, nulix, nulix-core), binaries (shellux, nulix, nulixctl, nulix-server), env vars (SHELLUX_*, NULIX_*), paths (~/.shelluxrc, ~/.shellux_history, nulix/nulix.conf, sockets), tests, and documentation (AGENTS.md, docs/). Verification results: `cargo fmt --all -- --check` clean, `cargo clippy --workspace --all-targets` clean (0 warnings), `cargo test --workspace` passed (115 repl tests, all unittests and doctests passed), `perl check.pl -p ./target/debug/epsh -s check-epsh.t` matched baseline (167 passed, 1 failed: regression-37), leftover grep clean outside excluded files. Leaves working tree uncommitted for review.
- **L4** (2026-11-22, parent verification): diff stays inside the allowed surfaces; 3 files renamed with git mv; no old names left outside S4 (also checked `SO1`, `so1_`, `so1-sh`). Re-ran: fmt clean, clippy clean, `cargo test --workspace` all green, conformance 167/1 (regression-37). Smoke test: `nulix-server` listens on `nulix/nulix-server-NAME.sock`, panes get `NULIX_SOCKET` (no `SO1TERM_SOCKET`), config diagnostics read `nulix: config FILE:LINE`, `SO1TERM_CONFIG` is ignored, `nulix`/`nulix-server`/`nulixctl` help texts and `shellux -c` work. Risk: none beyond the requested contract change (binary, env var and path names; nothing released yet).
- **L5** (2026-11-22): `epsh`'s internal prompt here-document delimiter was renamed by agy to `SHELLUX_PROMPT_EOF`; changed to the engine-neutral `EPSH_PROMPT_EOF` (445aeb8) so epsh names no embedding shell. Native review START refused with `lens_context_budget_exceeded` (39 files, ~4188 counted lines, renames counted whole; no authority created). User chose "Dejarlo sin revisión (Recommended)": the candidate stays unreviewed, verified by the parent checks in L4.

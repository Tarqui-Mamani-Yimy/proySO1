# so1term — TUI smoke matrix (sugPerms.md item 4)

Objective: catch so1term regressions with real full-screen programs, automated where the program is installed, plus a manual checklist for the rest.
Branch: `test/tui-smoke-matrix`. Delivery: merge to main on the user's request. Test runner: `cargo test -p so1-terminal --test tui`.

## Specs

- **S1** — sugPerms.md item 4 (verbatim): "**Matriz de pruebas TUI para so1term.** Esfuerzo: bajo." / "nvim, tmux, htop, fzf y lazygit: checklist manual y algunos tests sobre PTY."
- **S2** — Automated tests (design, orchestrator): a new integration test file drives a headless `so1term-server` session (same harness style as `crates/terminal/tests/server.rs`: private runtime dir, `SO1TERM_SHELL=/bin/sh`, `so1ctl send-text` / `get-text`, `wait_until` polling) and runs, when installed, `less`, `top`, `fzf` and `man`. For each program: it enters (expected screen content visible via `get-text`; for alternate-screen programs the shell prompt and earlier output are not visible while it runs), responds to keys (for example scrolling in `less`, typing a filter in `fzf`), quits with its normal key, and the pane returns to the shell with the earlier primary-screen output visible again and a new command working. Programs that are not installed make their test pass early with a printed "skipped: PROGRAM not installed" note (never fail). Environment is pinned (`TERM=xterm-256color`, `LANG=C.UTF-8`, a temp `HOME`, `LESS`/`MANPAGER`/`PAGER` set so the run is deterministic). No test depends on installing anything.
- **S3** — Manual checklist (design, orchestrator): a document listing, per program (nvim, tmux, htop, fzf, lazygit, less, top, man), the steps and expected results for so1term windows: alternate screen enter/exit, resize while running, mouse/scroll where applicable, kitty keyboard keys (Ctrl/Alt combos, Esc latency), colors, and that block navigation shortcuts pass through on the alternate screen. Location: `docs/tui-smoke-checklist.md`.
- **S4** — Docs: AGENTS.md Testing section lists the new test command and the checklist.

## Tasks

| ID | Specs | Route | Status | Commit |
|---|---|---|---|---|
| T1 — automated TUI tests, manual checklist, docs | S1–S4 | inline | done | see L4 |

## Log

- **L1** (2026-11-21, user): "vamos por el 3, y despues los demas" (item 4 follows item 3).
- **L2**: installed on this machine: top, fzf, less, man; missing: nvim, vim, tmux, htop, lazygit (not installed by the agent; installing is the user's decision).
- **L3** (2026-11-21, user): "resumen.md y sugerencias.md, no es necesario que vayan en el repo, ademas vamos con la siguiente tarea" — both added to `.git/info/exclude` (local only).
- **L4**: T1 done inline. `crates/terminal/tests/tui.rs`: less (page, G, q), top (h help, q, q), fzf (filter `ban` → `1/3`, Enter prints selection), man (`/SEE ALSO`, q); 4/4 pass, three consecutive runs. Finding: procps top does not switch to the alternate screen (no `?1049h`/`?47h`/`?1047h` under `script`), so its test checks the shell runs again instead of marker restoration. Checklist at `docs/tui-smoke-checklist.md`; AGENTS.md Testing updated.

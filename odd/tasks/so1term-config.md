# so1term — startup configuration file (sugPerms.md item 6, first part)

Objective: so1term and so1term-server read a `key = value` configuration file at startup (font, size, shell, scrollback, colours); hot reload is a later task.
Branch: `feat/so1term-config`. Delivery: merge to main on the user's request. Test runner: `cargo test -p so1-terminal` and `cargo test -p so1-term-core`.

## Specs

- **S0** — sugPerms.md item 6 (verbatim): "**Archivo de configuración de so1term.** Esfuerzo: medio." / "Primero carga al arrancar; la recarga en caliente después, aplicada por cliente y no al servidor de sesión."
- **S1** — Format chosen by the user: "clave = valor (Recommended)" — own parser, no new dependency, style of the so1sh policy file. One `key = value` per line; blank lines and lines starting with `#` ignored; surrounding whitespace trimmed.
- **S2** — Location (plan accepted with the format): `$SO1TERM_CONFIG` when non-empty, otherwise `$XDG_CONFIG_HOME/so1term/so1term.conf`, otherwise `$HOME/.config/so1term/so1term.conf`. A missing default file is silent; a missing or unreadable `$SO1TERM_CONFIG` prints one warning.
- **S3** — Keys: `font` (font file path), `font_size` (6–72), `shell` (program path), `scrollback` (lines, 0–100000, default 10000), `foreground`, `background`, `cursor` and `color0`..`color15` (`#rrggbb`). `SO1TERM_FONT`, `SO1TERM_FONT_SIZE` and `SO1TERM_SHELL` override the file.
- **S4** — An invalid line, unknown key or bad value prints `so1term: config FILE:LINE: MESSAGE` on stderr and keeps the default for that key; the window still opens.
- **S5** — Colours and fonts apply in the client (standalone window and `so1term --session`); `shell` and `scrollback` apply where the PTYs and models live (standalone window and `so1term-server`).
- **S6** — Docs: AGENTS.md section, `so1term --help`, and an example file.

## Tasks

| ID | Specs | Route | Status | Commit |
|---|---|---|---|---|
| T1 — parser + loader (`crates/terminal/src/config.rs`) with tests | S1–S4 | inline | done | see L5 |
| T2 — palette in term-core renderer, cursor colour, public scrollback limit | S3, S5 | inline | done | see L5 |
| T3 — wire window, client, server, PTY shell; docs | S3, S5, S6 | inline | done | see L5 |

## Log

- **L1** (2026-11-22, user): "si, haz el merge a la main y prosigue con el item Seis".
- **L2** (2026-11-22, user answer): format "clave = valor (Recommended)"; default plan for path, keys, env precedence, warnings and client-side colours shown in the same question and not objected to.
- **L3** findings: colours resolve in `term-core/src/renderer.rs` (`color_rgb`, hard-coded `ANSI`); attach frames carry symbolic `CellColor`, so a client palette works in `--session`. Cursor is drawn by inverting the cell. Named colours: 256 fg, 257 bg, 258 cursor, 259–266 dim, 267 bright fg. Shell override lives in `pty.rs` (`SO1TERM_SHELL`), scrollback in `Model::new` (10 000).
- **L4**: RED: `server_config_file_sets_the_shell_and_reports_bad_lines` timed out on base sources (stash of `crates/terminal/src`, `crates/term-core/src`). Palette and parser tests are new API (no runnable RED). The example file is parsed by a test, so it always equals the defaults. Test servers now set `SO1TERM_CONFIG=/dev/null` so a developer's own config never leaks in.
- **L5**: `cargo test --workspace` green; clippy clean. Not checked visually: fonts and colours in a real window (GPU; covered by palette unit tests and wiring review). Risk: item 3 — `SO1TERM_SHELL`/`SO1TERM_FONT_SIZE` handling and the public `PaneRenderInput::render_cells` signature are shared; env-wins test added, API callers are in-workspace only.
- **L6**: Independent verifier (Risk item 3): no blockers, S0–S6 PASS; default palette identical to the old `color_rgb` on all 65,792 colours. Applied its cheap notes with RED/GREEN tests: a UTF-8 BOM no longer breaks the first key, `color01`/`color+1` are rejected, `so1term-server --help` names XDG. Its probes became tests (CRLF, `=` in values, duplicate keys, value limits, exhaustive palette). Left as notes: leading `+` on numbers accepted; `~/` without HOME stays literal; empty `SO1TERM_FONT`/`SO1TERM_SHELL` count as set (shell already did); `pane_shells_inherit_the_server_socket` fails with a long TMPDIR (pre-existing, 80-column wrap).

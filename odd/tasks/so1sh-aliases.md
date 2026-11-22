# so1sh — interactive aliases (sugPerms.md item 3)

Objective: `alias`/`unalias` in the interactive so1sh REPL; epsh keeps no alias support (scripts, `-c` and piped input are unchanged).
Branch: `feat/so1sh-aliases`. Delivery: merge to main on the user's request. Test runner: `cargo test -p so1sh` (PTY tests in `crates/shell/tests/repl.rs`).

## Specs

- **S1** — sugPerms.md item 3 (verbatim): "**Alias solo en el REPL interactivo de so1sh.**" / "epsh sigue sin alias por diseño; la expansión vive en la capa de la shell." / "Tests para recursión, comillas y posición de palabra. Scripts, `-c` y entrada por pipe no expanden alias."
- **S2** — Builtins (design, orchestrator; POSIX `alias` semantics): `alias` lists every alias as `alias name='value'` sorted by name (single quotes escaped as `'\''`); `alias name` prints that one; `alias name=value ...` defines; an unknown name prints `alias: name: not found` to stderr and makes the status 1 (other operands still processed). `unalias name...` removes, `unalias -a` removes all; unknown name → `unalias: name: not found`, status 1. Names: POSIX alias-name characters only (letters, digits, `_`, `!`, `%`, `,`, `@`); any other name is rejected with `alias: invalid alias name: NAME`, status 1. Registered as so1sh embedder builtins in the interactive REPL only, like `jobs` and `sysinfo` (non-interactive modes go through `epsh::cli::run`, which offers no embedder hook; see L2).
- **S3** — Expansion (design, orchestrator; POSIX/dash): applied to each line typed at the interactive prompt (including PS2 continuation lines as one input) before epsh parses it. Only a word in command-name position of a simple command is checked (start of line and after `;`, `&`, `|`, `&&`, `||`, `(`, newline, and reserved words that begin a command list such as `then`, `do`, `else`, `elif`, `{`, `!`, `if`, `while`, `until`); the word must be an unquoted literal with no quoting, escaping or expansion. An alias is not expanded again inside its own expansion (no infinite recursion; `alias ls='ls -F'` works). When the value ends in a blank, the next word is also checked. History stores the line as typed. The rc file's alias definitions apply to later prompt lines; aliases are not expanded inside files sourced with `.` (documented).
- **S4** — Editor integration: alias names complete as commands and are highlighted like valid commands.
- **S5** — Docs: AGENTS.md (so1sh paragraph, architecture tree, and the "Aliases" bullet under "What's Not Implemented (by design)" clarified as epsh-only) and resumen.md.

## Tasks

| ID | Specs | Route | Status | Commit |
|---|---|---|---|---|
| T1 — alias/unalias builtins, REPL expansion, editor integration, docs | S1–S5 | delegated (context backstop) | done | see L2 |

## Log

- **L1** (2026-11-21, user): "vamos por el 3, y despues los demas"
- **L2**: Writer status partial → accepted. `crates/shell/src/aliases.rs` (table, builtins, expansion on epsh's lexer), REPL expansion before `eval_repl_line` with the typed line kept in history, completion and highlighting of alias names, AGENTS.md and resumen.md. RED observed (17/21 unit, completion/highlighter, 4/5 PTY). Deviation accepted by the orchestrator: builtins are REPL-only (S2 was the orchestrator's design, not a user requirement). Known limits: an alias whose value is an incomplete construct is not continued with PS2; words inside `$(...)` are never expanded; like POSIX shells, `unalias x; x` on one line still runs the alias.

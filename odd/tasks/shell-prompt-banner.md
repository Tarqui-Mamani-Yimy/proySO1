# Shell prompt with cwd and startup system banner
Objective: so1sh shows the current directory in its prompt and a fastfetch-style system banner when an interactive session starts.
Branch: `feat/shell-prompt-banner` (stacked on `feat/phase-5-advanced-terminal`). Delivery: `ask-on-risk`. Test runner: `cargo test -p so1sh < /dev/null`, `cargo test --workspace < /dev/null`.

## Specs
- **S1 — Current directory in the prompt.** User: "veo que en la terminal no se muestra mi direccion actual, y tengo que poner pwd, para saber en donde estoy".
  - `PS1` and `PS2` undergo POSIX parameter expansion before each prompt (today they are used literally, `crates/shell/src/repl.rs:95`), so `PS1='$PWD $ '` shows the directory and follows `cd`. An expansion error never ends the session: the prompt falls back to the literal value and the REPL continues. Invalid UTF-8 keeps the existing display-only replacement.
  - When `PS1` is unset, the default prompt is `user@host <cwd> (<git branch>) $ ` with `$HOME` abbreviated to `~`, `#` instead of `$` for root, the git branch read from `.git/HEAD` in the cwd or an ancestor (no git process; detached HEAD shows the short commit; outside a repo the part is omitted), and the trailing `$`/`#` in red after a non-zero last status. Colors only when stdout is a terminal and `NO_COLOR` is unset/empty.
  - Assumption: an explicitly set `PS1` (including `PS1='$ '`) keeps full user control; the default is only for unset `PS1`.
- **S2 — System banner at startup.** User: "quisiera que cuando se habra el proyecto, cargue el modelo de distro o SO que este ocupando el usuario, su espacio, ram, con shell trabaja, y si tienes algo mas que agregarle, seria bueno". Chosen style (user, 2026-11-19): "Completo con logo (Recomendado)" — small distro ASCII logo on the left, info on the right:
  ```
     /\        yimy@localhost
    /  \       ----------------
   /    \      OS:     Debian GNU/Linux 13
  /  /\  \     Kernel: 6.12.113+deb13-amd64
  \  \/  /     Shell:  so1sh 0.1.0
   \    /      Term:   so1term
    \  /       CPU:    AMD Ryzen 5 5600 (12)
     \/        RAM:    5.2 GiB / 15.5 GiB (34%)
               Disk /: 120 GiB / 457 GiB (26%)
               Uptime: 3h 12m
  ```
  - Shown once per interactive so1sh session, after the rc file and before the first prompt; never for piped input, `-c`, or scripts. `SO1SH_BANNER=0` (environment or rc) disables it.
  - Fields: `user@host`, OS pretty name (`/etc/os-release` `PRETTY_NAME`, macOS product name/version), kernel (`uname`), shell (`so1sh <crate version>`), terminal (`so1term` when `SO1TERM_SOCKET` is set, else `$TERM_PROGRAM`, else `$TERM`), CPU model and logical count, RAM used/total with percent (Linux `MemTotal - MemAvailable`), disk used/total with percent of `/` (`statvfs`), uptime. A field that cannot be read is omitted, never an error. Sizes in GiB with one decimal.
  - Logos: small built-in ASCII logos for at least Debian, Ubuntu, Arch, Fedora and a generic fallback, chosen from `/etc/os-release` `ID`/`ID_LIKE`; colored only under the same color rule as S1.
  - Builtin `sysinfo` prints the same banner on demand (registered through the embedder builtin hook, like `jobs`).
- **S3 — Extras (orchestrator, "si tienes algo mas que agregarle").** Git branch and failed-status color in the default prompt (S1); `sysinfo` builtin (S2). Nothing else is added.
- **S4 — Docs and tests.** AGENTS.md documents the prompt expansion, default prompt, banner, `SO1SH_BANNER` and `sysinfo`. Existing tests keep passing; PTY tests that rely on the old literal `PS1` keep setting it explicitly and banner-sensitive harnesses set `SO1SH_BANNER=0`.

## Tasks
- [x] **T1 — Prompt expansion and default prompt** (S1, S3, S4) — route: delegated writer (parallel with T2; long-session context backstop). `Shell::expand_prompt` (epsh, additive public API, parameter expansion only with heredoc-body quoting; command substitution/backticks/arithmetic rejected) + crates/shell/src/prompt.rs. RED: E0599 missing expand_prompt; 5/8 prompt unit tests failing with stubs; PTY $PWD and unset-PS1 tests failing on the old repl.rs. GREEN: epsh api_stability 20, so1sh unit 53, repl 99. Known edge: a prompt ending in a single backslash loses it.
- [x] **T2 — System banner and `sysinfo` builtin** (S2, S3, S4) — route: delegated writer (parallel with T1). crates/shell/src/sysinfo.rs with injectable sources, five logos, `sysinfo` builtin, `show_startup_banner`. RED 13 failing on todo!() stubs, GREEN 13. macOS branch not compiled here.
- [x] **T3 — Wire banner into startup, docs, smoke** (S2, S4) — route: parent inline. `sysinfo` registered next to `jobs`; banner shown after the rc file, before the first prompt. RED: PTY banner and builtin tests failed (2/3; suppression guard passed), GREEN 3/3. AGENTS.md documents prompt expansion, default prompt, banner, SO1SH_BANNER, sysinfo, new modules. Workspace: 1005 passed / 0 failed, fmt/clippy ok. X11 smoke screenshot: Debian logo banner with Term so1term, CPU, RAM 7.3/15.3 GiB, Disk / 93%; prompt `yimy@localhost ~/proyectos/so1Shell (feat/shell-prompt-banner) $`, `$` red after `false`.

## Log
- L1 (2026-11-19, user, verbatim): "despues haremos esa fase, quiero que hagamos una cosa mas bien, veo que en la terminal no se muestra mi direccion actual, y tengo que poner pwd, para saber en donde estoy, ademas quisiera que cuando se habra el proyecto, cargue el modelo de distro o SO que este ocupando el usuario, su espacio, ram, con shell trabaja, y si tienes algo mas que agregarle, seria bueno"
- L2 (2026-11-19): user chose "Completo con logo (Recomendado)". Finding: `PS1`/`PS2` are used literally today (`crates/shell/src/repl.rs:95`); PTY tests already set `PS1` explicitly (`crates/shell/tests/repl.rs:150`). Placement: shell layer per planing.md §2 (prompt and startup output belong to the shell).
- L3 (2026-11-19): T1-T3 done, evidence in Tasks. Next: native risk assessment of the feature commit; push/PR remain the user's decision.

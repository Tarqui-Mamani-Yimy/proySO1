# so1Shell — Interactive Shell on epsh

The workspace contains `epsh`, the non-interactive, embeddable POSIX engine,
and `shellux` (Shellux), a minimal interactive shell using that engine.
`shellux` uses reedline on a TTY, with `PS1` (default `$ `), `PS2` (default
`> `), persistent history, multiline input, and status-aware `exit` and EOF.
Variables/cwd/functions persist; Ctrl-R searches history, Ctrl-C clears input.
History uses `$SHELLUX_HISTFILE`, otherwise `$HOME/.shellux_history`; without either
variable it stays in memory. History I/O errors are reported without ending
the session. History paths retain OS bytes. Reedline accepts Unicode strings:
invalid UTF-8 in PS1/PS2 is replaced for display only, and interactive input
is Unicode-only; engine variables and non-interactive byte handling are unchanged.
Piped input, `-c`, and script arguments use the shared epsh CLI dispatcher.
Empty history variables are treated as unset; without a usable HOME, history
stays in memory. History I/O errors are reported without ending the session.
Foreground jobs use their own process group and terminal; Ctrl-C/Ctrl-\\ abort
the remaining interactive line (including builtin loops), returning with 130/131.
Ctrl-Z reports status 148 and keeps the stopped job in the job table
(`[n]+  Stopped  cmd`); `cmd &` registers a job and prints `[n] pgid`. The
`jobs` builtin (registered through the embedder builtin hook) lists jobs, with
`-l` for process groups and `%n`, `%+`, `%%`, `%-`, `%string`, `%?string` specs.
Job state changes are polled with waitpid on the job's own pids only. Terminal ownership and cooked modes are
restored after each job. Startup waits for foreground permission instead of
stealing the tty, and exit returns it to its original owner. Without a controlling
tty, one diagnostic is printed and editing continues without terminal job control.
If background stops are discarded, acquisition retries at 10 ms intervals and
falls back after 1 second without foreground permission. Modes are captured only
after foreground acquisition; fallback commands cannot take another owner's tty.
Before the first interactive prompt, startup sources non-empty `$SHELLUX_RC`, or
the existing `$HOME/.shelluxrc` when HOME is non-empty. It runs through `.` in the
persistent shell, so prompts, variables, functions and cwd can be configured.
Missing defaults are silent; explicit missing files and rc errors are reported
with normal REPL recovery (including `set -e` semantics). `exit` in the rc ends
the session. Piped input, `-c`, and script arguments never source an rc file.
When `SHELLUX_POLICY` is non-empty, the interactive REPL loads that policy file
before the rc file; a missing, unreadable or invalid file prints one diagnostic
(`shellux: policy FILE[:LINE]: ...`) and exits with status 2. One rule per line,
`#` comments (a `#` starting a word, so `/tmp/a#b` keeps it) and blank lines ignored: exactly one `default allow|deny`;
`allow|deny command NAME...` matches argv[0] or its basename; `allow|deny write
GLOB...` matches the lexically normalized absolute target of `>`, `>|`, `>>` and
`<>` with `epsh::glob::fnmatch`; a write pattern with an unclosed `[` is refused as invalid (write `\[` for a literal bracket). Command and write rules each use first match in
file order, then the default. Denied commands print `NAME: denied by policy` and
yield 126 without opening their redirections; the targets of `exec CMD` and
`command CMD` (also nested, as in `command exec CMD`) are checked the same way
before any redirection, so a denied `exec` leaves the shell's descriptors
unchanged, and commands run through `eval` are checked too. A denied write
prints `PATH: denied by policy` and aborts the rest of the line like any failed
redirection. A `default deny` policy also governs startup and exit: the rc file
is sourced through `.`, so it needs `allow command .`, and `exit` needs
`allow command exit`. Piped input, `-c` and scripts ignore `SHELLUX_POLICY`. It is not a sandbox:
programs that run other programs (`env`, `xargs`, `sh -c`, interpreters),
symlinks in write targets, and reads are not policed.
`PS1` and `PS2` undergo POSIX parameter expansion before each prompt (heredoc-body
quoting, as in dash), so `PS1='$PWD $ '` follows `cd`; command substitution,
backticks and arithmetic are rejected so drawing a prompt never runs code, and any
expansion error falls back to the literal value. With `PS1` unset the default
prompt is `user@host <cwd> (<git branch>) $ `: `$HOME` is abbreviated to `~`, root
gets `#`, the branch is read from `.git/HEAD` in the cwd or an ancestor (gitdir
files followed; detached HEAD shows 7 hex digits; omitted outside a repository),
and the `$`/`#` turns red after a non-zero status. Prompt and banner colours are
used only when stdout is a terminal and `NO_COLOR` is unset or empty. After the rc
file and before the first prompt, an interactive session prints a system banner
(distro ASCII logo for Debian, Ubuntu, Arch, Fedora or a generic one, then
user@host, OS, kernel, shell, terminal, CPU, RAM, disk usage of `/` and uptime;
unreadable fields are omitted). `SHELLUX_BANNER=0`, set in the environment or the
rc file, disables it; the `sysinfo` builtin prints it on demand.
Aliases live only in the interactive shell (epsh stays alias-free): `alias` lists
`alias name='value'` sorted by name, `alias name` prints one, `alias name=value`
defines, and `unalias name...`/`unalias -a` remove; unknown names report
`alias: name: not found` (or `unalias: ...`) with status 1, and names outside the
POSIX alias characters (letters, digits, `_ ! % , @`) report
`alias: invalid alias name: NAME`. Each line typed at the prompt (PS2 continuation
lines included) is expanded before epsh parses it: only an unquoted literal word in
command-name position (line start, after `; & | && || (`, a newline, or `if then
else elif do while until { !`), never quoted or escaped words, `$(...)`, comments or
here-document bodies. An alias is not re-expanded inside its own expansion, and a
value ending in a blank makes the next word a candidate. A whole line is expanded
before it runs, so `unalias x; x` still expands `x`. History keeps the typed line;
rc-file definitions apply at later prompts, but files sourced with `.` (including
the rc file itself) are never expanded. Alias names complete and highlight as
commands. The builtins are registered by the REPL only, so `-c`, scripts and piped
input neither define nor expand aliases.
`fg %n` resumes a job in the foreground and `bg %n` in the background; before each
prompt shellux reports changed background jobs (`[n]+ Done/Stopped/Killed`), and
`exit` with stopped jobs warns once. `exec CMD` replaces the shell with CMD and
passes prefix assignments (`FOO=bar exec cmd`) to it; a failed exec exits a
non-interactive shell (127 not found, 126 otherwise) and the REPL recovers with
that status as `$?`.

## Architecture

```
Cargo.toml        Virtual workspace: crates/epsh, crates/shell, crates/term-core, and crates/terminal
crates/term-core/
  src/lib.rs      Headless terminal state library (nulix-core) and public module declarations
  src/model.rs    VT model, snapshots, selection state, and OSC/block integration
  src/blocks.rs   Shell-integration block state and stable grid anchors
  src/block_navigation.rs Block navigation targets and retained-grid text extraction
  src/layout.rs   Pure pane split tree, pixel/cell geometry, focus, and tab state
  src/osc.rs      Incremental OSC 7 / OSC 133 observer over PTY bytes
  src/apc.rs      Bounded incremental APC payload observer over PTY bytes
  src/keyboard.rs Kitty keyboard encoding, paste helpers, and IME preedit gate
  src/graphics.rs Kitty graphics protocol parsing, bounded decoding, and image storage
  src/placements.rs Per-pane kitty placement state, deletion selectors, and viewport projection
  src/renderer.rs Renderer-independent grid, color, block, and image projection helpers
  src/control.rs Versioned JSON-lines control request/response types and parser
crates/shell/
  src/main.rs     shellux binary, selects the REPL for TTY input
  src/repl.rs     Startup rc, reedline editor, validator, prompts and history
  src/terminal.rs Interactive-only process group, signal policy and tty modes
  src/jobs.rs     Job table, job specs and the `jobs`, `fg` and `bg` builtins
  src/aliases.rs  Alias table, `alias`/`unalias` builtins and REPL-line alias expansion
  src/completion.rs Tab completion of commands, paths and variables
  src/highlighter.rs Syntax highlighting from the epsh lexer (SHELLUX_HIGHLIGHT=0 disables it)
  src/history_hint.rs Ghost-text hints from history
  src/policy.rs   Opt-in `SHELLUX_POLICY` command/write policy parser and hooks
  src/prompt.rs   PS1/PS2 expansion and the default prompt (cwd, git branch, status)
  src/sysinfo.rs  Startup system banner, distro logos and the `sysinfo` builtin
  tests/repl.rs   PTY end-to-end interactive tests (rexpect)
  tests/cli.rs    Non-interactive CLI compatibility tests
crates/terminal/
  src/main.rs     nulix window/event loop, per-pane PTYs/models, input routing, and GPU renderer
  src/lib.rs      Public term-core re-exports, shell selection, and surface helpers
  src/config.rs   Startup `key = value` configuration file (fonts, shell, scrollback, palette)
  src/control.rs  Unix-domain control transport, peer credentials, and nulixctl client transport
  src/bin/nulixctl.rs Local control API CLI
  src/bin/nulix-server.rs Persistent headless session server binary
  src/session.rs  Headless session (tabs, panes, PTY + VT model) owned by nulix-server
  src/graphics_pipeline.rs Shared kitty graphics pipeline (APC split, ingest, decode, placements) for window and server panes
  src/remote.rs   nulix --session client: server autostart, attach stream, RemoteView frame store
  src/pty.rs      portable-pty lifecycle, reader thread, resize, and teardown
  src/selection.rs Click tracking and pure mouse-selection routing
crates/epsh/src/
  lib.rs          Public API, module declarations
  main.rs         Thin epsh CLI binary entry point
  cli.rs          Shared option/script/stdin dispatch for epsh and shellux
  ast.rs          AST types: Command, Word, WordPart, Redir, ParamExpr
  lexer.rs        Single-pass lexer with unified word-part builder
  parser.rs       Recursive-descent parser, heredoc body reading
  eval.rs         Shell struct, ShellBuilder, command evaluation, pipelines, command substitution
  builtins.rs     31 builtins: echo, cd, test, read, set, trap, printf, kill, ...
  expand.rs       Word expansion: tilde, parameter, arithmetic, field splitting, pathname expansion, patterns
  arith.rs        $((…)) evaluator with short-circuit (noeval), int var cache
  var.rs          Variable storage with scope stack, integer cache
  glob.rs         Custom fnmatch + pathname expansion (cwd-aware)
  redirect.rs     FD save/restore for redirections
  signal.rs       Signal handler install/reset, atomic pending flags
  test_cmd.rs     POSIX test/[ recursive-descent evaluator
  error.rs        ShellError enum, ExitStatus newtype, Result alias
  encoding.rs     PUA-based byte preservation for non-UTF-8 data
  sys.rs          Thin libc wrappers
```

Kitty graphics ingestion accepts direct RGB, RGBA, PNG, guarded file, and guarded temporary-file data with bounded base64/zlib decoding. Images are capped at 128 MiB per pane and 512 MiB per process, with dimensions capped at 10,000 per axis and oldest-image eviction. File and temporary-file transmission are denied unless `NULIX_GRAPHICS_ALLOWED_ROOTS` lists one or more roots using the platform path-list separator; empty and non-existent roots are ignored, and existing roots are canonicalized once at startup. Files must be absolute regular files beneath an allowed root, with bounded size/offset; Unix opens each canonical path component relative to a retained root descriptor with `O_NOFOLLOW` and verifies opened-file identity. In-root symlinks are allowed only when their canonical target remains in an allowed root. Temporary files must additionally be beneath the canonical system temp root or an allowed root, include `tty-graphics-protocol` in the full path, and are deleted only after a guarded successful read and a matching inode check. Windows currently denies both file media because an equivalent no-follow guard is not implemented. Shared memory remains unsupported. Display actions `a=T` and `a=p` are enabled with per-pane GPU texture caching and pane-clipped placement rendering. Negative z-index images render beneath text; z below -1073741824 also renders beneath cell backgrounds; non-negative z renders above text, with placement order breaking ties. Placements use stable-row anchors, follow scrollback, are scoped to primary/alternate screens, invalidate on width reflow, and support kitty delete selectors; uppercase deletes free unreferenced image data and their GPU textures are released on the next redraw. Cursor movement follows the placement rectangle unless `C=1`. Unicode placeholders and animation remain unsupported, as does shared memory; Windows file transmission remains denied.

## Nulix Local Control

On Unix, each running `nulix` instance listens on a private Unix-domain socket. The socket is placed at `$XDG_RUNTIME_DIR/nulix/nulix-<pid>.sock` when `XDG_RUNTIME_DIR` is non-empty, otherwise under `<temp-dir>/nulix-<uid>/nulix-<pid>.sock`. The containing directory must be owned by the current user and have no group/other permissions (created as mode 0700); the socket is mode 0600 and is removed on normal exit or SIGTERM. Each peer's effective user ID is checked before requests are served. The protocol is versioned JSON-lines, currently v1, with one request per line and a 1 MiB line limit; it is local-only and never uses TCP. Event-loop requests have a bounded 30-second timeout.

`NULIX_SOCKET` is inherited by every pane shell and tells `nulixctl` which instance to address. Set `NULIX_CONTROL=0` before launching `nulix` to disable the listener. `nulixctl` supports `list`, `new-tab`, `split`, `focus`, `close`, `send-text`, `get-text` (screen or last finished block output), `block`, `wait`, and `shutdown` (only `nulix-server` accepts it); use `nulixctl --help` for argument syntax. `list` reports for every pane `state` (`unknown` before any shell-integration mark, `idle` at a prompt, `running` between OSC 133 `C` and `D`) and `last_exit_status` (the newest finished command's status, or null). `block [--pane ID] [--id N]` describes one block (default: the newest block that ran a command) as `pane`, `id`, `state`, `command`, `cwd`, `exit_status`, `started_at_ms`, `finished_at_ms` and `duration_ms` (Unix milliseconds, null when unknown); when the command text is no longer retained, `command` is null and `command_unavailable` is `anchors-missing` or `alternate-screen`. Unknown ids return `NOT_FOUND` and blocks discarded with their scrollback return `EVICTED`. `wait [--pane ID] [--after ID] [--timeout SECS]` is client-side: it polls `block` every 50 ms until the newest command block (newer than `--after`, when given) has finished, prints it, and exits 1 with `TIMEOUT` after SECS (default 600). Each request gets at least 250 ms, so `--timeout 0` still probes once; transport errors (for example a missing socket) are reported as they are, never as `TIMEOUT`. To wait for a command you send, read `block` first and pass its id as `--after`, so the previous block is never mistaken for the new one. `send-text` writes supplied UTF-8 bytes to the selected PTY, while `get-text` returns visible grid text or anchored last-block output. Sensitive text payloads and results are not logged. Windows has no v1 listener or named-pipe transport; `nulixctl` reports that local control is unsupported there.

## Nulix Configuration

At startup `nulix` and `nulix-server` read a `key = value` file (`crates/terminal/src/config.rs`, no dependency): `$NULIX_CONFIG` when non-empty, else `$XDG_CONFIG_HOME/nulix/nulix.conf`, else `$HOME/.config/nulix/nulix.conf`. Blank lines and lines starting with `#` are ignored (colours start with `#`, so there are no end-of-line comments). Keys: `font` (font file; a leading `~/` is relative to HOME), `font_size` (6-72), `shell`, `scrollback` (0-100000 lines, default 10000), `foreground`, `background`, `cursor` (block cursor fill; unset inverts the cell) and `color0`..`color15`, all colours `#rrggbb`. `NULIX_FONT`, `NULIX_FONT_SIZE` and `NULIX_SHELL` override the file. A missing default file is silent; a missing explicit file, a line without `=`, an unknown key or a bad value prints `nulix: config FILE[:LINE]: MESSAGE` and that key keeps its default, so the window still opens. Fonts and colours apply in the window (also as a `--session` client, whose frames carry symbolic colours); `shell` and `scrollback` apply where panes live (the standalone window or `nulix-server`). Changes need a restart; hot reload is not implemented. Example: `docs/nulix.conf.example`.

## Nulix Persistent Sessions

`nulix-server [--session NAME]` runs a headless session that owns its shell PTYs and terminal models and outlives every client. NAME defaults to `default` and must be 1-64 ASCII letters, digits, `-` or `_` (otherwise exit status 2). The server listens on `<runtime dir>/nulix-server-<NAME>.sock` with the same directory, permission, peer-uid, and stale-socket rules as the window listener, and exports that path as `NULIX_SOCKET` to its pane shells. Clients such as `nulixctl --socket PATH` may connect and disconnect at any time while the shells keep running (detach). Starting a second server for a live session name fails with `session NAME is already running` (exit status 1); a socket left by a killed server is detected as stale and replaced. The server exits with status 0 and removes its socket when its last shell exits or on `nulixctl shutdown`, which terminates the remaining shells; a nulix window refuses `shutdown`. It ignores SIGHUP so closing the launching terminal does not end the session, and SIGTERM unlinks the socket. Panes use a virtual 80x24 grid of 8x16 px cells until an attached client supplies geometry, which then persists after detach. Server panes run the same kitty graphics pipeline as window panes (`graphics_pipeline.rs`): replies go to the pane PTY, `NULIX_GRAPHICS_ALLOWED_ROOTS` applies to the server process, and decoding runs off the server loop. Windows reports that persistent sessions are unsupported.

A control `attach` request upgrades its connection to the attach stream (`crates/term-core/src/attach.rs`, JSON lines up to 16 MiB, no read timeout). Only one client may be attached; another attach gets `operation-refused` "another client is attached to session NAME". After the client's `Hello` the server adopts its pixel and cell size, replies `Welcome`, then sends `Layout` and full per-pane `Frame`s. The server owns view state (scrollback position, selection, block navigation) and sends per-pane frames with sequence numbers: deltas carry only changed rows, while cursor, selection, input modes, block decorations, title and cwd are always complete; full frames follow attach, resize, tab switches, `Resync` and dropped deltas. Each client has a reader and a writer thread; the writer queue holds at most 64 frames / 8 MiB (excess deltas are dropped and the pane gets a full frame) and 1024 other messages (beyond that the client is detached). When the session ends an attached client receives `Exit` (`shutdown` or `last-shell-exited`) before the server exits, and attaches racing the shutdown are refused; nulixctl requests keep working while a client is attached.

`nulix --session NAME` opens the window as the session's attached client (`nulix --help` lists the option; an invalid NAME exits with status 2). It connects to the session socket, or starts `nulix-server --session NAME` (the binary next to `nulix`, else from `PATH`) in a new session with null stdio and waits up to 5 seconds for it to listen. It attaches once its window size has been unchanged for 150 ms, so a window manager's resize right after mapping never reaches the server as a provisional `Hello` size (a width change reflows the panes and drops kitty placements). The window keeps the standalone keybindings (pane, tab, block, copy/paste), but routes their actions to the server. It owns no PTYs, models or control listener: it renders the server's layout and per-pane frames through the same renderer input as the standalone window, encodes keys locally with the focused frame's keyboard modes, and sends input, resize, scroll, selection, block, copy and tab/pane actions to the server. Closing the window detaches and exits 0 while the session keeps running; a server `Exit` closes the window, and a broken stream closes it with one diagnostic (status 1). Client mode shows kitty graphics: frames carry the projected placements, and before a frame places an image the server sends an `image` announcement and its RGBA data in contiguous chunks of at most 4 MiB, once per image per attached client (again after re-attach); `image-removed` reports freed or replaced data. Image lines are never dropped. A new image starts only while less than 256 MiB of earlier image lines are queued for the client, and a started image is queued whole, so a single image larger than 256 MiB still reaches the client; a client whose backlog is still above 256 MiB when another image must start gets an `IMAGE_BACKLOG` error and is detached. The client checks announced sizes, chunk contiguity, the 10,000 px dimension limit and a 512 MiB total, skips placements whose image is still incomplete, and renders the rest with the standalone window's texture cache and z-order code; textures are released when images are removed or their pane goes away. Plain `nulix` remains the standalone window.

## Embedding API

```rust
use epsh::eval::{CommandPolicyDecision, Shell};
use epsh::parser::Parser;
use epsh::builtins::{is_builtin, BUILTIN_NAMES};

// Builder pattern — recommended for embedders
let mut shell = Shell::builder()
    .cwd(PathBuf::from("/project"))
    .errexit(true)
    .pipefail(true)
    .noglob(true)
    .noexec(false)
    .stdout_sink(stdout.clone())
    .stderr_sink(stderr.clone())
    .cancel_flag(cancel.clone())
    .timeout(Duration::from_secs(120))
    .env_clear()
    .build();

// Parse once, execute separately (for permission checking)
let program = Parser::new("echo hello").parse().unwrap();
let status = shell.run_program(&program);

// Custom process spawner (for sandboxing, job control, SSH proxy)
shell.set_external_handler(Box::new(|argv, prefix_assignments| {
    // argv[0] is the command name; prefix_assignments are variable assignments
    // redirections already applied to file descriptors;
    // epsh::eval::external_command_cwd() is the shell's working directory
    todo!("spawn the process your way")
}));

// Optional: own the open(2) of writable redirections (>, >|, >>, <>).
// Return None to keep epsh's default open, or the descriptor you opened
// with exactly the requested semantics (for example after preserving the
// existing file). Earlier redirections are restored if one fails.
shell.set_redirect_open_handler(Box::new(|request| {
    let _ = (request.path, request.mode, request.fd);
    None
}));

// Optional: refuse commands before they run. Called with the expanded argv
// of every simple command (before function/builtin/external dispatch and
// before its redirections open, including the target of `exec CMD` and
// `command CMD`) and again inside those builtins. Deny writes `<argv0>: denied by policy[: <reason>]` to the
// shell's stderr route and yields 126. Also `ShellBuilder::command_policy`.
shell.set_command_policy(Box::new(|argv| {
    if argv[0].as_bytes() == b"rm" {
        CommandPolicyDecision::Deny(String::from("agents may not delete"))
    } else {
        CommandPolicyDecision::Allow
    }
}));

// Interactive shell primitives
let mut ish = Shell::builder()
    .interactive(true)           // enables tcsetpgrp + WUNTRACED
    .external_handler(handler)   // you own fork/exec/wait
    .build();
// ShellError::Stopped { pid, pgid } propagates when a process is stopped
// Frontends without job resume may opt into cleanup; host jobs are never adopted.
ish.discard_stopped_jobs();

// Builtin detection for permission systems / prompt coloring
assert!(is_builtin("echo"));
assert!(!is_builtin("rm"));
```

## Testing

```sh
cargo test --workspace
cargo test -p shellux --test repl                        # PTY REPL tests
cargo test -p nulix --test tui                           # TUI smoke matrix (less, top, fzf, man; skips missing)
cargo run -p shellux                                     # interactive on a TTY
make ci                                               # both crates + conformance
cargo test -p epsh --test api_stability                      # API surface regression
cargo test -p epsh --test embedding                          # embedding API tests
cargo test -p epsh --test integration                        # shell behavior tests
cargo build && perl check.pl -p ./target/debug/epsh \
  -s check-epsh.t                                            # mksh conformance
sh crates/epsh/tests/stress/run.sh ./target/debug/epsh dash  # performance vs dash
perl filter-tests.pl check.t > check-epsh.t                  # regenerate filtered tests
```

Manual nulix checks for nvim, tmux, htop, fzf, lazygit, less, top and man
(resize, mouse, kitty keys, colours, shortcut pass-through) live in
`docs/tui-smoke-checklist.md`.

## Performance vs dash

Fork-dominated operations (typical coding agent workload) are at parity:

| Operation | vs dash | Technique |
|-----------|---------|-----------|
| Builtin command substitution | **3.6x faster** | Fork-free `$(echo ...)` |
| Heredocs | **0.8x** | Faster than dash |
| Pipelines | **1.1x** | posix_spawn, exec-direct |
| External commands | **1.1x** | At parity |
| Tight `[ ... ]` + arith loops | ~2x | Measured in release, 300k iterations |

The old ~28x loop gap was not arithmetic: an unquoted lone `[` triggered
pathname expansion (a directory read) on every `[ ... ]` test. `[` is now a
pattern only when a `]` closes it in the same path component, as in dash;
arithmetic and assignment were already within ~1.3x of dash.

## Design Lineage — What We Actually Took

The original plan drew from three reference shells. Here's what we ended up
using, what we adapted, and where we diverged.

### From dash (primary — semantics and behavior)

dash is the conformance target. We ported these mechanisms directly:

- **EV_TESTED errexit model** (`eval.rs`): Only leaf nodes (Simple, Pipeline, Subshell)
  trigger `set -e` exit. Compound nodes propagate status but never exit directly.
  The `tested` flag suppresses errexit in if/while/until conditions and `&&`/`||`
  left operands. Matches `eval.c` lines 246-330.

- **pgetc_eatbnl** (`lexer.rs`): `peek()` and `advance()` transparently consume
  `\<newline>` continuations. `peek_raw()`/`advance_raw()` for single-quote and
  heredoc body contexts. Comments use raw reading.

- **parsesub syntax context** (`lexer.rs`): `read_brace_param` passes `in_dquote`
  to `read_brace_word_parts`. Trim ops (`%`, `#`) force BASESYNTAX (single quotes
  are quoting). Default/assign/alt (`-`, `+`, `=`, `?`) inherit context. Inner `"`
  in dquote `${…}` toggles context (dash's `innerdq`).

- **Heredoc reading at newlines** (`parser.rs`): `read_pending_heredocs` called
  after newline consumption in both `parse_program` and `parse_command_list`.
  Matches dash's `parseheredoc()` in `readtoken()`.

- **in_forked_child optimization** (`eval.rs`): Subshells inside pipeline children
  execute directly without double-forking. Prevents pipe fd leaks. Mirrors dash's
  `EV_EXIT` fast-path in `evalsubshell`.

- **IO_NUMBER detection** (`parser.rs`): Digit-only unquoted words before `<`/`>`
  parsed as fd numbers, not arguments.

- **Arithmetic noeval** (`arith.rs`): Ternary `?:` and `&&`/`||` suppress side
  effects (assignments) on non-taken branches via `noeval` flag. Matches dash's
  `arith_yacc.c`.

- **preglob/RMESCAPE_GLOB** (`expand.rs`): `expand_pattern()` escapes glob
  metacharacters from quoted regions for fnmatch. Used by trim ops and case patterns.

- **Heredoc body rules** (`parser.rs`, `lexer.rs`): Unquoted heredocs use DQSYNTAX
  (single quotes literal, `"` literal, only `\$` `\`` `\\` `\<nl>` are special
  backslash escapes). `\<newline>` continuation with odd/even backslash counting.

### From posh (IFS and expansion)

- **IFS field-splitting state machine** (`expand.rs`): Single-pass `field_split`
  distinguishes IFS whitespace vs non-whitespace. Handles empty fields between
  non-WS delimiters, leading/trailing WS stripping. Adopted as designed.

- **in_param_word flag** (`expand.rs`): Literals inside `${var+word}` etc. are
  marked `split_fields: true` when in unquoted context. This was our adaptation
  of posh's SubType stack concept — we don't have a stack, but the flag achieves
  the same per-context splitting behavior.

- **Variable scope stack** (`var.rs`): `push_scope()`/`pop_scope()` with saved
  values for `local`. Inspired by posh's environment stacking, implemented with
  Rust's RAII-friendly design.

### From mrsh (AST design and API)

- **Span on every AST node** (`ast.rs`): Source position tracking for error
  reporting. Used in ShellError::Syntax and ShellError::Runtime.

- **word_break metadata** (`expand.rs`): `ExpandedWord` carries `word_break: bool`
  to mark `"$@"` field boundaries, keeping arguments separate through field splitting.

- **Library-first architecture**: `Shell` struct with `run_script`, `set_var`,
  `get_var` API. The binary is a thin CLI wrapper.

### Where We Diverged

- **Single-pass WordPart building** — we started with mrsh's two-phase approach
  (lexer collects raw text, parser re-parses into WordParts) but hit a wall of
  quoting-context bugs at ~74% conformance. We restructured to build WordParts
  directly during lexing (matching dash's single-pass model) while keeping
  mrsh's typed enum representation instead of dash's control-character-in-string
  approach. This was the highest-impact architectural change.

- **ExitStatus newtype** — neither dash, posh, nor mrsh distinguish exit codes
  at the type level. Our `ExitStatus(i32)` with named constants (`SUCCESS`,
  `FAILURE`, `MISUSE`, `NOT_FOUND`, `NOT_EXECUTABLE`) and `from_wait()` prevents
  integer confusion throughout the codebase.

- **Expansion returns Result** — dash uses setjmp/longjmp, posh uses longjmp,
  mrsh uses return codes. We use `Result<T, ShellError>` with the `?` operator,
  which gives us dash-equivalent error propagation with Rust's type safety.
  BadSubst, `${var?msg}`, and arithmetic errors all propagate cleanly.

- **`$(<file)` optimization** — reads the file directly without forking, since
  we can detect the pattern (Simple command with no args/assigns, one input
  redirect) before entering the fork path.

## Interactive Shell Support

epsh doesn't implement interactive features directly, but provides the minimal
primitives for an interactive shell to be built on top:

- **`ExternalHandler`**: callback replacing fork+exec for external commands.
  The embedder owns process creation for job control and terminal management.
- **`interactive` mode**: `tcsetpgrp` gives pipeline foreground, `WUNTRACED`
  detects stopped processes, `ShellError::Stopped { pid, pgid }` carries
  the pipeline PGID for job resume.
- **`BUILTIN_NAMES`**: for prompt coloring and completion.

Everything else (prompt, history, line editing, `fg`/`bg`/`jobs`) is the
interactive shell's responsibility.

shellux emits terminal shell-integration metadata when running interactively and
stdout is the acquired controlling terminal. OSC 7 reports the current working
directory before each prompt; OSC 133 uses `A` for prompt start, `B` for the
end of the primary prompt/start of input, `C` before each accepted command
line (including an empty line), and `D;<status>` after it finishes. A line
cancelled with Ctrl-C before acceptance emits no `C` or `D`; the next prompt
starts a fresh block. Multiline PS2 input remains within one block. Metadata is
written to the terminal, not through shell output or command redirections.
Sequences use the 7-bit ST terminator (`ESC \\`). Set
`SHELLUX_SHELL_INTEGRATION=0` to disable all shell-integration sequences.
On Unix interactive TTYs, shellux also enables reedline's kitty keyboard
enhancement when the terminal answers its bounded capability probe. Set
`SHELLUX_KITTY_KEYBOARD=0` to opt out; terminals that do not answer continue
with legacy key handling after the probe timeout. Windows shell-side kitty
keyboard consumption is not currently supported. The enhancement is popped
before shell commands run so child programs receive the terminal's prior mode.

nulix enables alacritty_terminal's kitty keyboard parser and exposes the
active per-screen flags to its key encoder. With no flags, key bytes remain on
the existing xterm path. Kitty mode supports disambiguation, key event types,
alternate key codes, all-keys CSI-u reporting, and associated text. winit 0.30
provides cross-platform Shift/Alt/Ctrl/Super state plus tracked CapsLock and
NumLock; it does not expose Hyper or Meta state portably, so those modifier
bits are currently always clear in GUI-originated events. Lock state starts
unknown and is tracked from key events while the window is open. This implements the
terminal side only; shell-side protocol consumption is Phase 5 T2.

In `nulix`, Ctrl+Shift+Up/Down (Cmd+Up/Down on macOS) navigates to the
previous/next anchored block prompt. Ctrl+Shift+A (Cmd+Shift+A on macOS)
selects the block output at or below the viewport focus, falling back to the
most recent finished block. Ctrl+Shift+O copies that selected block's output;
Ctrl+Shift+L copies its command line. These bindings do not overlap the
existing Ctrl+Shift+C/V copy/paste bindings. Block copying refuses blocks whose
required start/end anchors were evicted or whose marks belong to the alternate
screen; no partial text is silently presented as a complete block.

Block navigation is a temporary viewport pin: any key input, a new primary
prompt mark, or scrolling back to the live bottom resumes live-follow. ED 2
clears anchors for erased rows but does not change the shell block's semantic
state; a running block remains Running until its D mark, and cleared rows are
not re-anchored. While the alternate screen is active, block navigation/copy
shortcuts pass through to the application and block decorations are hidden.
Alacritty/vte 0.26 recognizes DECSET 1049 but not the common 47/1047 aliases;
nulix normalizes those aliases to 1049 screen switching so TUI output remains
on the alternate grid.

Pane layout uses `Horizontal` for left/right splits and `Vertical` for top/bottom
splits. Ratios are the first child's share. Visible pane dimensions are floored
to whole cells; divider pixels belong to neither pane and are not hit-tested.
Splits are refused unless both children can meet the configured minimum. On a
shrink, the focused branch is kept visible if it meets minimum size, otherwise
the first fitting branch is shown; panes that cannot fit are marked hidden, and
if the whole window is below minimum all panes are hidden. Arrow focus chooses
the smallest edge gap, then greatest perpendicular overlap, nearest
perpendicular center, top/left position, and PaneId. Pane IDs are globally
monotonic across trees. Tab navigation wraps; closing the active tab selects the tab now occupying
its index, or the previous final tab when the removed tab was last.

Each pane owns one PTY, VT `Model`, view state, wakeup gate, and glyph cache;
PTY output notifications carry its globally unique `PaneId`. `Ctrl+Shift+D`
splits right, `Ctrl+Shift+E` splits down, `Ctrl+Shift+W` closes the focused pane,
and `Ctrl+Alt+Arrow` moves focus by geometry. On macOS, use `Cmd+Shift+D/E/W`
and `Cmd+Option+Arrow`. Tabs use a single-row bar at the top only while more
than one tab exists; the bar takes one terminal cell row from every tab's PTY
grid. `Ctrl+Shift+T` creates a tab, `Ctrl+Shift+Q` closes the active tab,
`Ctrl+Tab` / `Ctrl+Shift+Tab` cycle tabs, and `Alt+1..9` selects a numbered tab
only when it targets a different existing tab; otherwise the key is sent to the
focused shell. On macOS use `Cmd+Shift+T/Q` and `Cmd+1..9`; Ctrl+Tab cycling
remains unchanged.
`Ctrl+Shift+W` still closes only the focused pane, and closes its tab if that was
the tab's last pane. A tab label is the active pane's OSC 7 cwd basename, or
`shellux` until a cwd mark arrives. OSC 0/2 titles override the cwd basename for
the active pane's tab label and window title; a terminal title reset returns to
the cwd basename. The active tab uses a stronger accent background and brighter
text. Hidden tabs continue draining and applying PTY output but do not request
redraws. The final pane of the final tab exits the window. These bindings are
reserved by nulix and do not replace block navigation (`Ctrl+Shift+Up/Down`).

### Embedder Builtins

Applications can register shell-local builtins on a `Shell` instance with
`register_embedder_builtin(name, callback)`. The callback receives `&mut Shell`
and byte-preserving `argv` (`argv[0]` is the command name), and returns the
normal `Result<ExitStatus, ShellError>`. Use `Shell::write_stdout` and
`Shell::write_stderr` for output so configured stdout/stderr sinks are honored;
redirections are installed before the callback runs. An active redirection on
fd 1 or 2 takes precedence over its configured sink; callback writes then use
the redirected descriptor. Registered names are
reported by `command -v`, `command -V`, and `type`. Query registrations with
`Shell::is_embedder_builtin_registered`; `builtins::is_builtin` and
`BUILTIN_NAMES` continue to describe epsh's builtins only.

Lookup follows POSIX command precedence: special builtins first, then shell
functions, then regular epsh builtins, embedder builtins, and PATH. Functions
take precedence over regular epsh builtins as well as embedder builtins; POSIX
special builtins retain precedence over functions. By default, registering
an epsh builtin name fails. Applications may explicitly opt into shadowing a
regular epsh builtin with `register_embedder_builtin_shadowing_regular`; POSIX
special builtins are never shadowable. The existing `ExternalHandler` remains a
fallback for otherwise-unresolved names before epsh's PATH executor, preserving
its established catch-all behavior.

## Agent Discoverability and Shell Terminology

Coding agents navigate by filenames and text search. Keep names distinctive,
use one spelling per concept, and put important explanations above the
definitions they describe. Name tests after the source or behavior they cover;
update definitions, call sites, tests, and docs together during renames.

Use shell terms precisely:

- A token is a lexical unit; a shell word is an unexpanded sequence of word
  parts; a field is an argument produced after expansion.
- A simple command has assignments, a command name, arguments, and redirections.
- A pipeline joins commands with `|`; an AND-OR list uses `&&` or `||`; a
  sequential list uses `;` or a newline. Use list as the umbrella term.
- Parameter expansion handles `$HOME`; pathname expansion, or globbing, handles
  `*.rs`; command substitution handles `$(...)` and backticks.
- A builtin runs in the shell; an external command runs through the external
  handler or default process executor.

`epsh` may parse more constructs than an embedding application exposes. Treat
the embedding contract and tests as authoritative, not the dependency's full
parser capability.

## Known Limitations

- Traps follow dash/POSIX timing: a trapped signal runs its action as soon as
  the current command completes (also inside builtin-only loops), with `$?`
  restored afterwards. A foreground command, pipeline or command substitution is
  never cut short; only the `wait` builtin is, returning 128+signal before the
  trap runs. Forked subshells reset caught traps; ignored signals stay ignored.
  The `read` builtin is not interrupted by traps.
- A background simple command (`cmd &`) forks a subshell that runs `cmd` as its
  child, so `$!` names the subshell; `kill $!` does not reach `cmd` (dash execs it)
- `~user` expansion requires getpwnam (not available stdlib-only)

## What's Not Implemented (by design)

- Job control builtins (fg, bg, jobs) — use `external_handler` + `Stopped`
- Interactive features (prompt, history, line editing) — use rustyline/reedline
- Aliases in epsh — shellux expands them in its interactive REPL only (`crates/shell/src/aliases.rs`)
- Here-strings (`<<<`)
- Extended globs (`@(...)`, `+(...)`, etc.)
- Arrays, typeset, select (ksh extensions)

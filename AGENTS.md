# so1Shell — Interactive Shell on epsh

The workspace contains `epsh`, the non-interactive, embeddable POSIX engine,
and `so1sh` (working name), a minimal interactive shell using that engine.
`so1sh` uses reedline on a TTY, with `PS1` (default `$ `), `PS2` (default
`> `), persistent history, multiline input, and status-aware `exit` and EOF.
Variables/cwd/functions persist; Ctrl-R searches history, Ctrl-C clears input.
History uses `$SO1SH_HISTFILE`, otherwise `$HOME/.so1sh_history`; without either
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
Before the first interactive prompt, startup sources non-empty `$SO1SH_RC`, or
the existing `$HOME/.so1shrc` when HOME is non-empty. It runs through `.` in the
persistent shell, so prompts, variables, functions and cwd can be configured.
Missing defaults are silent; explicit missing files and rc errors are reported
with normal REPL recovery (including `set -e` semantics). `exit` in the rc ends
the session. Piped input, `-c`, and script arguments never source an rc file.
Job resume (`fg`/`bg`) and done/stopped notifications are later Phase 2 tasks.

## Architecture

```
Cargo.toml        Virtual workspace: crates/epsh, crates/shell, and crates/terminal
crates/shell/
  src/main.rs     so1sh binary, selects the REPL for TTY input
  src/repl.rs     Startup rc, reedline editor, validator, prompts and history
  src/terminal.rs Interactive-only process group, signal policy and tty modes
  src/jobs.rs     Job table, job specs and the `jobs` builtin
  tests/repl.rs   PTY end-to-end interactive tests (rexpect)
  tests/cli.rs    Non-interactive CLI compatibility tests
crates/terminal/
  src/main.rs     so1term window/event loop, PTY wiring, and GPU renderer
  src/lib.rs      Public module declarations, shell selection, and surface helpers
  src/blocks.rs   OSC 133 command-block state and grid anchors
  src/keyboard.rs Xterm key encoding, paste helpers, and IME preedit gate
  src/model.rs    alacritty_terminal adapter, snapshots, selections, and write-back events
  src/osc.rs      Incremental OSC 7 / OSC 133 observer over PTY bytes
  src/pty.rs      portable-pty lifecycle, reader thread, resize, and teardown
  src/renderer.rs Grid/color helpers and pure block-overlay projection
  src/selection.rs Click tracking and pure mouse-selection routing
crates/epsh/src/
  lib.rs          Public API, module declarations
  main.rs         Thin epsh CLI binary entry point
  cli.rs          Shared option/script/stdin dispatch for epsh and so1sh
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

## Embedding API

```rust
use epsh::eval::Shell;
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
cargo test -p so1sh --test repl                        # PTY REPL tests
cargo run -p so1sh                                     # interactive on a TTY
make ci                                               # both crates + conformance
cargo test -p epsh --test api_stability                      # API surface regression
cargo test -p epsh --test embedding                          # embedding API tests
cargo test -p epsh --test integration                        # shell behavior tests
cargo build && perl check.pl -p ./target/debug/epsh \
  -s check-epsh.t                                            # mksh conformance
sh crates/epsh/tests/stress/run.sh ./target/debug/epsh dash  # performance vs dash
perl filter-tests.pl check.t > check-epsh.t                  # regenerate filtered tests
```

## Performance vs dash

Fork-dominated operations (typical coding agent workload) are at parity:

| Operation | vs dash | Technique |
|-----------|---------|-----------|
| Builtin command substitution | **3.6x faster** | Fork-free `$(echo ...)` |
| Heredocs | **0.8x** | Faster than dash |
| Pipelines | **1.1x** | posix_spawn, exec-direct |
| External commands | **1.1x** | At parity |
| Tight arith loops | ~28x | String allocation dominated |

The arith loop gap requires integer variable representation (dash's approach).

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

so1sh emits terminal shell-integration metadata when running interactively and
stdout is the acquired controlling terminal. OSC 7 reports the current working
directory before each prompt; OSC 133 uses `A` for prompt start, `B` for the
end of the primary prompt/start of input, `C` before each accepted command
line (including an empty line), and `D;<status>` after it finishes. A line
cancelled with Ctrl-C before acceptance emits no `C` or `D`; the next prompt
starts a fresh block. Multiline PS2 input remains within one block. Metadata is
written to the terminal, not through shell output or command redirections.
Sequences use the 7-bit ST terminator (`ESC \\`). Set
`SO1SH_SHELL_INTEGRATION=0` to disable all shell-integration sequences.

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

- Signal traps fire between commands but not during blocking waits
- `~user` expansion requires getpwnam (not available stdlib-only)
- Arith loops ~28x slower than dash (string allocation; fixable with integer var repr)

## What's Not Implemented (by design)

- Job control builtins (fg, bg, jobs) — use `external_handler` + `Stopped`
- Interactive features (prompt, history, line editing) — use rustyline/reedline
- Aliases
- Here-strings (`<<<`)
- Extended globs (`@(...)`, `+(...)`, etc.)
- Arrays, typeset, select (ksh extensions)

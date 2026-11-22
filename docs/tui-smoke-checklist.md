# so1term TUI smoke checklist

Manual checks for full-screen programs in a real `so1term` window. The automated
counterpart, `cargo test -p so1-terminal --test tui`, covers `less`, `top`, `fzf`
and `man` in a headless `so1term-server` pane; this list covers what it cannot
see: window resize, mouse, kitty keyboard keys, colours and so1term shortcuts.

## How to run

1. Build and start a window: `cargo run -p so1-terminal --bin so1term`.
2. In the shell, run `echo before-42` so there is primary-screen output to check.
3. For each program below, run every common check, then its specific checks.
4. Repeat once in a persistent session (`so1term --session smoke`) for at least
   `nvim` and `tmux`, and detach/re-attach while the program is running.

A check passes only when the expected result is observed; note the program
version and the failing step for anything else.

## Common checks (every program)

| Check | Steps | Expected |
|---|---|---|
| Enter | Start the program | It fills the window; `before-42` is hidden (except `top`, which redraws the primary screen) |
| Exit | Quit with the program's normal key | The prompt returns; `before-42` is visible again (`top` leaves its last frame); a new command works |
| Resize | Resize the window, then split a pane with Ctrl+Shift+D | The program redraws to the new size with no stale rows or wrapped garbage |
| Colours | Look at the program's highlighted UI | 256-colour/truecolour themes render; no missing or swapped colours |
| Esc latency | Press Esc once | It acts immediately (no visible delay before the program reacts) |
| Shortcuts pass through | Press Ctrl+Shift+Up/Down and Ctrl+Shift+A inside the program | The program receives them; so1term does not jump between blocks or select block output, and no block decorations appear |
| Scroll after exit | After quitting, scroll up | Scrollback holds the shell output, not the program's alternate screen |

## Program-specific checks

| Program | Steps | Expected |
|---|---|---|
| nvim | `nvim`, type text in insert mode, `:vsplit`, Ctrl+W then arrow keys, `:set mouse=a` and click/scroll, Ctrl+V for a block selection, `:q!` | Keys and modifiers arrive as typed; mouse clicks move the cursor, the wheel scrolls the buffer; cursor shape changes between normal and insert mode |
| tmux | `tmux`, Ctrl+B `%` and Ctrl+B `"` to split, Ctrl+B arrows, `set -g mouse on` then click panes and drag dividers, run `nvim` inside, `exit` until tmux ends | Splits and borders draw correctly; mouse selects panes and resizes them; nested nvim keys still arrive; tmux status bar keeps its colours |
| htop | `htop`, F2 setup, arrow keys, click a column header, scroll the process list with the wheel, F10 | Function keys work; mouse selects and sorts; meters keep their colours |
| fzf | `ls \| fzf`, type a filter, Up/Down, Ctrl+J/Ctrl+K, scroll with the wheel, Enter; then `fzf --height 40%` | Filtering updates live; selection prints after Enter; the `--height` mode draws inline below the prompt without hiding earlier output |
| lazygit | `lazygit` in a repository, Tab between panels, Enter on a file, `?` for help, click a panel, `q` | Panels redraw on resize; mouse focuses panels; Unicode box-drawing and icons render |
| less | `less` on a long file, Space/b, `/pattern`, `n`, `G`, mouse wheel, `q` | Paging and search work; the wheel scrolls the file |
| top | `top`, `h` for help, `q` to leave help, `1` for per-CPU rows, `q` | Help appears and closes; the screen refreshes every interval without flicker |
| man | `man ls`, `/SEE ALSO`, `n`, `q` | Bold and underline render; search jumps to the match |

## Kitty keyboard keys

In nvim (`:map` unused combinations, or `nvim -c 'autocmd InsertCharPre * echo v:char'`)
and in a shell with the kitty keyboard enhancement active, check:

- Ctrl+Shift+letter, Alt+letter and Ctrl+Alt+letter arrive as distinct keys.
- Ctrl+I versus Tab, Ctrl+M versus Enter and Ctrl+[ versus Esc are distinguished
  by programs that enable disambiguation (nvim does).
- Releasing keys does not insert characters in programs that did not ask for
  release events.

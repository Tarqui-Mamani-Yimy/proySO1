# nulix up — demo guide

A fixed, rehearsable script for showing Shellux and Nulix to a jury in about
ten minutes. Follow the steps in order; every command is ready to copy. Linux
only (macOS and Windows builds are not released yet).

## Before the demo

1. Build release binaries once and put them first on `PATH` for this terminal:

   ```sh
   cargo build --release -p shellux -p nulix
   export PATH="$PWD/target/release:$PATH"
   ```

2. Optional extras (skip any you will not show):
   - A policy file for step 8, saved as `~/demo.policy`:

     ```text
     default allow
     deny command rm
     deny write /etc/*
     ```

   - An image tool that speaks the kitty graphics protocol for step 9, such as
     `chafa` (`sudo apt install chafa`), and any PNG file.
3. Rehearse once on the projector: check the font size (`font_size` in
   `~/.config/nulix/nulix.conf`, see `docs/nulix.conf.example`) and that the
   window fills the screen.

## The demo

### 1. Open the friendly mode (30 s)

```sh
nulix up
```

Point out the system banner, the prompt (user, host, folder, git branch), the
tab bar at the top and the shortcut bar at the bottom. Plain `nulix` opens the
same terminal without these extras, for people who already know the keys.

### 2. Commands become cards (1 min)

```sh
ls
false
sleep 5
```

- `ls`: grey card with its duration and a green ✓.
- `false`: red border and `✗ 1`, the exit status.
- `sleep 5`: blue border and `● 1.0s, 2.0s, ...` while it runs, then ✓.

### 3. Built-in help (20 s)

Press **F1**: every shortcut, grouped. Press **Esc** to close it.

### 4. Panes and tabs (1 min)

- **Ctrl+Shift+D** splits right, **Ctrl+Shift+E** splits down.
- **Ctrl+Alt+Arrows** moves focus; **Ctrl+Shift+W** closes a pane.
- **Ctrl+Shift+T** or the **+** button opens a tab; click a tab to switch,
  click **×** to close it.

### 5. Working with blocks (1 min)

- **Ctrl+Shift+Up/Down** jumps between commands.
- **Ctrl+Shift+A** selects a command's output (accent border).
- **Ctrl+Shift+O** copies that output, **Ctrl+Shift+L** copies the command.

### 6. A terminal that agents can drive (2 min)

Split right (**Ctrl+Shift+D**) and, in the new pane:

```sh
nulixctl list
nulixctl send-text --pane 1 'echo hola desde otro panel
'
nulixctl wait --pane 1
nulixctl get-text --pane 1 --scope last-block
```

`list` shows every pane's state and last exit status; `send-text` types into
another pane; `wait` blocks until that pane's command finishes and prints its
result; `get-text` reads its output. This is the local control API meant for
coding agents.

### 7. Sessions that survive the window (1 min)

```sh
nulix --session demo
```

Run `sleep 300 &` or any command, close the window, then run
`nulix --session demo` again: the shells kept running and the screen comes
back. (Sessions use the classic look; `nulix up --session` is not supported
yet.)

### 8. Shellux, the shell itself (2 min)

- **Tab** completes commands and paths; typing shows grey suggestions from
  history; **Ctrl+R** searches history.
- `alias ll='ls -l'` then `ll`.
- `sleep 100`, **Ctrl+Z**, `jobs`, `fg`, then **Ctrl+C**.
- Policy (needs the file from "Before the demo"):

  ```sh
  SHELLUX_POLICY=~/demo.policy shellux
  rm anything        # rm: denied by policy (status 126)
  echo x > /etc/x    # /etc/x: denied by policy
  exit
  ```

### 9. Images in the terminal (30 s, optional)

```sh
chafa -f kitty photo.png
```

## If something goes wrong

- **A symbol shows as a box**: the font lacks it; set a font with wide Unicode
  coverage in `nulix.conf`.
- **The window does not open on Wayland**: try `env -u WAYLAND_DISPLAY nulix up`
  to use XWayland.
- **A step fails live**: move on to the next one; every step is independent.

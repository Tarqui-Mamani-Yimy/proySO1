//! TUI smoke matrix: real full-screen programs inside a headless `nulix-server` pane.
//!
//! Each test runs one installed program (`less`, `top`, `fzf`, `man`) in a pane
//! driven through the real `nulixctl` binary: the program must take over the screen
//! (alternate screen, so earlier shell output is hidden; `top` redraws the
//! primary screen instead), react to keys, quit with
//! its normal key, and hand the pane back to the shell with the earlier output
//! visible again and a new command working. A program that is not installed makes
//! its test pass early with a "skipped" note; nothing is ever installed.
//! The manual counterpart for nulix windows is `docs/tui-smoke-checklist.md`.
#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

const SERVER: &str = env!("CARGO_BIN_EXE_nulix-server");
const CTL: &str = env!("CARGO_BIN_EXE_nulixctl");
const WAIT: Duration = Duration::from_secs(15);
const SESSION: &str = "tui";
/// Printed before each program starts; hidden while it owns the alternate screen.
const BEFORE: &str = "before-42";
const BEFORE_COMMAND: &str = "echo before-$((6*7))\n";

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

fn installed(program: &str) -> bool {
    let found = env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|dir| {
            fs::metadata(dir.join(program))
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
    });
    if !found {
        println!("skipped: {program} not installed");
    }
    found
}

/// One headless session with a pinned, deterministic environment.
struct Pane {
    root: PathBuf,
    server: Child,
}

impl Pane {
    fn start() -> Self {
        let root = env::temp_dir().join(format!(
            "nulix-tui-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home/.config")).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.join("home");
        let server = Command::new(SERVER)
            .args(["--session", SESSION])
            .env("XDG_RUNTIME_DIR", &root)
            .env("NULIX_SHELL", "/bin/sh")
            // Never read the developer's own nulix.conf.
            .env("NULIX_CONFIG", "/dev/null")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("PS1", "$ ")
            .env("LESS", "")
            .env("LESSHISTFILE", "-")
            .env("PAGER", "less")
            .env("MANPAGER", "less")
            .env("MANWIDTH", "80")
            .env("FZF_DEFAULT_OPTS", "")
            .env_remove("FZF_DEFAULT_COMMAND")
            .env_remove("LESSOPEN")
            .env_remove("LESSCLOSE")
            .env_remove("NO_COLOR")
            .env_remove("NULIX_SOCKET")
            .current_dir(&home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pane = Self { root, server };
        wait_until("server socket", || pane.socket().exists());
        pane
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn socket(&self) -> PathBuf {
        self.root
            .join("nulix")
            .join(format!("nulix-server-{SESSION}.sock"))
    }

    fn ctl(&self, args: &[&str]) -> Output {
        Command::new(CTL)
            .arg("--socket")
            .arg(self.socket())
            .args(args)
            .env_remove("NULIX_SOCKET")
            .output()
            .unwrap()
    }

    fn send(&self, text: &str) {
        let output = self.ctl(&["send-text", text]);
        assert!(
            output.status.success(),
            "send-text failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn screen(&self) -> String {
        let output = self.ctl(&["get-text"]);
        if !output.status.success() {
            return String::new();
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        value["text"].as_str().unwrap_or_default().to_owned()
    }

    fn wait_for(&self, what: &str, mut condition: impl FnMut(&str) -> bool) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let screen = self.screen();
            if condition(&screen) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; screen:\n{screen}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_text(&self, needle: &str) -> String {
        self.wait_for(needle, |screen| screen.contains(needle))
    }

    /// Prints the marker that the program's alternate screen must hide.
    fn print_marker(&self) {
        self.send(BEFORE_COMMAND);
        self.wait_text(BEFORE);
    }

    /// The program owns the screen: `needle` is shown and the marker is hidden.
    fn wait_program(&self, needle: &str) {
        self.wait_for(&format!("{needle} on the alternate screen"), |screen| {
            screen.contains(needle) && !screen.contains(BEFORE)
        });
    }

    /// The shell is back: the marker reappears and a new command runs.
    fn assert_shell_restored(&self) {
        self.wait_text(BEFORE);
        self.assert_shell_runs();
    }

    fn assert_shell_runs(&self) {
        self.send("echo after-$((40+2))\n");
        self.wait_text("after-42");
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn less_scrolls_and_quits_back_to_the_shell() {
    if !installed("less") {
        return;
    }
    let pane = Pane::start();
    let lines: String = (1..=200).map(|n| format!("line-{n:03}\n")).collect();
    fs::write(pane.home().join("lines.txt"), lines).unwrap();
    pane.print_marker();
    pane.send("less lines.txt\n");
    pane.wait_program("line-001");
    pane.send(" ");
    pane.wait_for("less to page forward", |screen| {
        !screen.contains("line-001") && screen.contains("line-030")
    });
    pane.send("G");
    pane.wait_text("line-200");
    pane.send("q");
    pane.assert_shell_restored();
}

#[test]
fn top_shows_help_and_quits_back_to_the_shell() {
    if !installed("top") {
        return;
    }
    // procps top redraws the primary screen instead of switching to the
    // alternate one, so its last frame stays visible after it quits.
    let pane = Pane::start();
    pane.send("top -d 1\n");
    pane.wait_text("load average");
    pane.send("h");
    pane.wait_text("Help for Interactive Commands");
    pane.send("q");
    pane.wait_for("top to leave its help screen", |screen| {
        screen.contains("load average") && !screen.contains("Help for Interactive Commands")
    });
    pane.send("q");
    pane.assert_shell_runs();
}

#[test]
fn fzf_filters_and_returns_the_selection_to_the_shell() {
    if !installed("fzf") {
        return;
    }
    let pane = Pane::start();
    pane.print_marker();
    pane.send(
        "printf 'apple\\nbanana\\ncherry\\n' | fzf > pick.txt; echo picked-$(cat pick.txt)\n",
    );
    pane.wait_program("cherry");
    pane.send("ban");
    pane.wait_for("fzf to filter", |screen| {
        screen.contains("1/3") && !screen.contains("cherry")
    });
    pane.send("\r");
    pane.wait_text("picked-banana");
    pane.assert_shell_restored();
}

#[test]
fn man_searches_and_quits_back_to_the_shell() {
    if !installed("man") {
        return;
    }
    let has_page = Command::new("man")
        .args(["-w", "ls"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !has_page {
        println!("skipped: man page for ls not installed");
        return;
    }
    let pane = Pane::start();
    pane.print_marker();
    pane.send("man ls\n");
    pane.wait_program("NAME");
    pane.send("/SEE ALSO\r");
    pane.wait_for("less to jump to the search match", |screen| {
        screen.contains("SEE ALSO") && !screen.contains("SYNOPSIS")
    });
    pane.send("q");
    pane.assert_shell_restored();
}

//! End-to-end tests for the `so1term-server` persistent session server (Phase 5 T10).
//!
//! Each test runs the real server binary with an isolated `XDG_RUNTIME_DIR` and
//! drives it through the real `so1ctl` binary over the private control socket.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

const SERVER: &str = env!("CARGO_BIN_EXE_so1term-server");
const CTL: &str = env!("CARGO_BIN_EXE_so1ctl");
const WAIT: Duration = Duration::from_secs(10);

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

/// Scratch runtime directory outside the worktree, removed on drop.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "so1term-server-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }

    fn socket(&self, session: &str) -> PathBuf {
        self.root
            .join("so1term")
            .join(format!("so1term-server-{session}.sock"))
    }

    fn server(&self, args: &[&str]) -> Command {
        let mut command = Command::new(SERVER);
        command
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.root)
            .env("SO1TERM_SHELL", "/bin/sh")
            .env_remove("SO1TERM_SOCKET")
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn spawn(&self, session: &str) -> Server {
        let child = self.server(&["--session", session]).spawn().unwrap();
        let server = Server { child };
        wait_until("server socket", || self.socket(session).exists());
        server
    }

    fn ctl(&self, session: &str, args: &[&str]) -> Output {
        Command::new(CTL)
            .arg("--socket")
            .arg(self.socket(session))
            .args(args)
            .env_remove("SO1TERM_SOCKET")
            .output()
            .unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Server {
    child: Child,
}

impl Server {
    fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "server did not exit in time");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.is_running() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "so1ctl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(stdout(output).trim()).unwrap()
}

fn screen_contains(scratch: &Scratch, session: &str, needle: &str) -> bool {
    let output = scratch.ctl(session, &["get-text"]);
    output.status.success() && json(&output)["text"].as_str().unwrap().contains(needle)
}

fn is_socket(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

#[test]
fn server_startup_creates_private_socket_and_one_pane() {
    let scratch = Scratch::new();
    let mut server = scratch.spawn("alpha");
    let socket = scratch.socket("alpha");
    assert!(is_socket(&socket));
    assert_eq!(
        fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let list = json(&scratch.ctl("alpha", &["list"]));
    let tabs = list["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0]["panes"].as_array().unwrap().len(), 1);
    assert!(server.is_running());
}

#[test]
fn server_keeps_panes_running_after_clients_detach() {
    let scratch = Scratch::new();
    let mut server = scratch.spawn("detach");
    stdout(&scratch.ctl("detach", &["send-text", "x=$((20+22)); echo detached-$x\n"]));
    // The sending client has disconnected; a later client still sees the shell output.
    wait_until("shell output", || {
        screen_contains(&scratch, "detach", "detached-42")
    });
    stdout(&scratch.ctl("detach", &["new-tab"]));
    let list = json(&scratch.ctl("detach", &["list"]));
    assert_eq!(list["tabs"].as_array().unwrap().len(), 2);
    assert!(server.is_running());
}

#[test]
fn pane_shells_inherit_the_server_socket() {
    let scratch = Scratch::new();
    let _server = scratch.spawn("env");
    stdout(&scratch.ctl(
        "env",
        &["send-text", "printf 'sock=%s\\n' \"$SO1TERM_SOCKET\"\n"],
    ));
    let expected = format!("sock={}", scratch.socket("env").display());
    wait_until("socket variable", || {
        screen_contains(&scratch, "env", &expected)
    });
}

#[test]
fn server_exits_when_the_last_shell_exits() {
    let scratch = Scratch::new();
    let mut server = scratch.spawn("exit");
    stdout(&scratch.ctl("exit", &["send-text", "exit\n"]));
    let status = server.wait_exit();
    assert!(status.success(), "unexpected status {status:?}");
    assert!(!scratch.socket("exit").exists());
}

#[test]
fn shutdown_command_stops_the_server_and_removes_the_socket() {
    let scratch = Scratch::new();
    let mut server = scratch.spawn("stop");
    let pid_file = scratch.root.join("shell.pid");
    stdout(&scratch.ctl(
        "stop",
        &[
            "send-text",
            &format!("echo $$ > '{}'\n", pid_file.display()),
        ],
    ));
    wait_until("shell pid file", || {
        fs::read_to_string(&pid_file).is_ok_and(|text| text.ends_with('\n'))
    });
    let shell_pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    stdout(&scratch.ctl("stop", &["split", "--direction", "right"]));
    let reply = json(&scratch.ctl("stop", &["shutdown"]));
    assert_eq!(reply["stopping"], serde_json::json!(true));
    let status = server.wait_exit();
    assert!(status.success(), "unexpected status {status:?}");
    assert!(!scratch.socket("stop").exists());
    // The pane shell was terminated with the session, not left running.
    wait_until("shell termination", || unsafe {
        libc::kill(shell_pid, 0) != 0
    });
}

#[test]
fn a_live_session_name_is_refused() {
    let scratch = Scratch::new();
    let mut first = scratch.spawn("dup");
    let output = scratch.server(&["--session", "dup"]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("session dup is already running"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(first.is_running());
    assert!(json(&scratch.ctl("dup", &["list"]))["tabs"].is_array());
}

#[test]
fn a_killed_server_leaves_a_stale_socket_that_a_restart_recovers() {
    let scratch = Scratch::new();
    let mut crashed = scratch.spawn("recover");
    crashed.child.kill().unwrap();
    crashed.child.wait().unwrap();
    assert!(
        is_socket(&scratch.socket("recover")),
        "SIGKILL leaves the socket"
    );
    let mut restarted = scratch.spawn("recover");
    // spawn() only waits for the path, which the stale socket already satisfies.
    wait_until("restarted server", || {
        scratch.ctl("recover", &["list"]).status.success()
    });
    assert!(restarted.is_running());
}

#[test]
fn invalid_session_names_are_rejected() {
    let scratch = Scratch::new();
    for name in ["", "../escape", "a b", &"x".repeat(65)] {
        let output = scratch.server(&["--session", name]).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "name {name:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("invalid session name"),
            "name {name:?}"
        );
    }
}

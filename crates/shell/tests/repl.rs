use std::process::Command;

use rexpect::process::WaitStatus;
use rexpect::session::{PtySession, ReadUntil, spawn_command};
use std::sync::{Mutex, MutexGuard};
use tempfile::TempDir;

#[test]
fn pty_timeout_override_is_respected() {
    let _pty_test_guard = serialize_pty_test();
    assert_eq!(
        pty_timeout_from_override(Some("17")),
        std::time::Duration::from_secs(17)
    );
    for value in [Some("invalid"), Some("0"), None] {
        assert_eq!(
            pty_timeout_from_override(value),
            std::time::Duration::from_secs(DEFAULT_PTY_TIMEOUT_SECS)
        );
    }
}

const DEFAULT_PTY_TIMEOUT_SECS: u64 = 20;
static PTY_TEST_LOCK: Mutex<()> = Mutex::new(());

fn pty_timeout_from_override(value: Option<&str>) -> std::time::Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|&seconds| seconds > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or_else(|| std::time::Duration::from_secs(DEFAULT_PTY_TIMEOUT_SECS))
}

fn pty_test_timeout() -> std::time::Duration {
    pty_timeout_from_override(std::env::var("SO1SH_TEST_TIMEOUT_SECS").ok().as_deref())
}

fn pty_deadline() -> std::time::Instant {
    std::time::Instant::now() + pty_test_timeout()
}

fn serialize_pty_test() -> MutexGuard<'static, ()> {
    PTY_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_pty(command: Command) -> PtySession {
    let timeout = pty_test_timeout();
    let millis = timeout.as_millis().min(u64::MAX as u128) as u64;
    let mut session = spawn_command(command, Some(millis)).unwrap();
    session.process_mut().set_kill_timeout(Some(millis));
    session
}

struct ProcessGroupCleanup(Option<libc::pid_t>);

impl ProcessGroupCleanup {
    fn new(pgid: libc::pid_t) -> Self {
        Self(Some(pgid))
    }

    fn terminate(&self) {
        if let Some(pgid) = self.0 {
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

fn background_pgid(launch: &[u8]) -> libc::pid_t {
    let plain = String::from_utf8(strip_ansi_escapes::strip(launch)).unwrap();
    plain
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|pair| {
            (pair[0] == "[1]")
                .then(|| pair[1].parse::<libc::pid_t>().ok())
                .flatten()
        })
        .expect("background launch should report job number and process group")
}

impl Drop for ProcessGroupCleanup {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn wait_for_process_group(pids: &[i32], pgid: i32) {
    let deadline = pty_deadline();
    loop {
        let joined = pids
            .iter()
            .all(|&pid| unsafe { libc::getpgid(pid) } == pgid);
        if joined {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pipeline members {pids:?} did not join process group {pgid}"
        );
        std::thread::yield_now();
    }
}

fn wait_for_job_state(session: &mut PtySession, expected: &str) -> String {
    let deadline = pty_deadline();
    loop {
        submit(session, "jobs");
        let output = session.exp_string("$ ").unwrap();
        let listing = String::from_utf8(strip_ansi_escapes::strip(output)).unwrap();
        if listing.contains(expected) {
            return listing;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "job never reached {expected:?}; last listing: {listing:?}"
        );
        std::thread::yield_now();
    }
}

fn session() -> (PtySession, TempDir) {
    session_with(&[], None)
}

fn session_with(args: &[&str], prompt: Option<&str>) -> (PtySession, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_so1sh"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "dumb")
        .env("SO1SH_SHELL_INTEGRATION", "0")
        .env("SO1SH_HIGHLIGHT", "0")
        .env("HOME", dir.path())
        .current_dir(dir.path());
    command.args(args);
    if let Some(prompt) = prompt {
        command.env("PS1", prompt);
    }
    // rexpect disables terminal echo, so output assertions cannot match typed input.
    let session = spawn_pty(command);
    (session, dir)
}

fn session_with_shell_integration(value: Option<&str>) -> (PtySession, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_so1sh"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "dumb")
        .env("HOME", dir.path())
        .env("SO1SH_HIGHLIGHT", "0")
        .current_dir(dir.path());
    if let Some(value) = value {
        command.env("SO1SH_SHELL_INTEGRATION", value);
    }
    (spawn_pty(command), dir)
}

const OSC133_A: &str = "\x1b]133;A\x1b\\";
const OSC133_B: &str = "\x1b]133;B\x1b\\";
const OSC133_C: &str = "\x1b]133;C\x1b\\";
const OSC133_D0: &str = "\x1b]133;D;0\x1b\\";
const OSC133_D1: &str = "\x1b]133;D;1\x1b\\";

fn expect_sequence(session: &mut PtySession, sequence: &str) -> String {
    let (_, matched) = session
        .exp_any(vec![ReadUntil::String(sequence.to_owned())])
        .unwrap();
    assert_eq!(matched, sequence);
    matched
}

fn capture_primary_prompt(session: &mut PtySession) -> String {
    let (before_prompt, matched) = session
        .exp_any(vec![ReadUntil::String(OSC133_A.to_owned())])
        .unwrap();
    assert_eq!(matched, OSC133_A);
    let visible_prompt = session.exp_string("$ ").unwrap();
    let matched_input = expect_sequence(session, OSC133_B);
    format!("{before_prompt}{matched}{visible_prompt}{matched_input}")
}

fn assert_sequences_in_order(output: &str, sequences: &[&str]) {
    let mut offset = 0;
    for sequence in sequences {
        let next = output[offset..]
            .find(sequence)
            .unwrap_or_else(|| panic!("missing {sequence:?} after byte {offset}: {output:?}"));
        offset += next + sequence.len();
    }
}

// Consume the editor's echoed submission before checking real command output.
fn submit(session: &mut PtySession, line: &str) {
    session.send_line(line).unwrap();
    session.exp_string("\r\n").unwrap();
}

fn assert_no_output(output: &str) {
    assert!(strip_ansi_escapes::strip(output).is_empty(), "{output:?}");
}

fn encoded_local_hostname() -> String {
    let mut hostname = [0u8; 256];
    assert_eq!(
        unsafe { libc::gethostname(hostname.as_mut_ptr().cast(), hostname.len()) },
        0
    );
    let hostname = &hostname[..hostname.iter().position(|&byte| byte == 0).unwrap_or(256)];
    let mut encoded = String::new();
    for &byte in hostname {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn exited(session: &mut PtySession, code: i32) {
    session.exp_eof().unwrap();
    assert!(
        matches!(session.process().wait().unwrap(), WaitStatus::Exited(_, actual) if actual == code)
    );
}

#[test]
fn echo_then_prompt() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo hi");
    assert_no_output(&session.exp_string("hi\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn variables_persist_across_lines() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "x=5");
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo $x");
    assert_no_output(&session.exp_string("5\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn exit_with_explicit_status() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit 3");
    exited(&mut session, 3);
}

#[test]
fn control_d_exits_successfully() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session.send_control('d').unwrap();
    exited(&mut session, 0);
}

#[test]
fn cwd_and_functions_persist_across_lines() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, dir) = session();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    session.exp_string("$ ").unwrap();
    session
        .send_line("cd sub; f() { echo persistent; }")
        .unwrap();
    session.exp_string("$ ").unwrap();
    session.send_line("pwd; f").unwrap();
    session
        .exp_string(&format!("{}/sub\r\npersistent\r\n", dir.path().display()))
        .unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn previous_status_and_implicit_exit() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "false");
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo $?");
    assert_no_output(&session.exp_string("1\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "false");
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 1);
}

#[test]
fn eof_preserves_status_and_defers_exit_trap() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "trap 'echo goodbye' EXIT");
    let before_prompt = session.exp_string("$ ").unwrap();
    assert_no_output(&before_prompt);
    submit(&mut session, "false");
    session.exp_string("$ ").unwrap();
    session.send_control('d').unwrap();
    session.exp_string("goodbye\r\n").unwrap();
    exited(&mut session, 1);
}

#[test]
fn syntax_error_updates_status_and_keeps_prompt() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "fi");
    session.exp_string("epsh: ").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo $?");
    assert_no_output(&session.exp_string("2\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn prompt_can_change_and_nested_exit_is_honored() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "PS1='custom> '");
    session.exp_string("custom> ").unwrap();
    session
        .send_line("f() { exit 7; }; f; echo unreachable")
        .unwrap();
    let output = session.exp_eof().unwrap();
    assert!(!output.contains("\r\nunreachable\r\n"));
    assert!(matches!(
        session.process().wait().unwrap(),
        WaitStatus::Exited(_, 7)
    ));
}

#[test]
fn inherited_ps1_is_used() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session_with(&[], Some("inherited> "));
    session.exp_string("inherited> ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn command_string_on_tty_does_not_enter_repl() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session_with(&["-c", "echo hi; exit 3"], None);
    assert_no_output(&session.exp_string("hi\r\n").unwrap());
    exited(&mut session, 3);
}

#[test]
fn explicit_exit_defers_exit_trap_and_preserves_requested_status() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session
        .send_line("trap 'echo goodbye; false' EXIT")
        .unwrap();
    let output = session.exp_string("$ ").unwrap();
    assert_no_output(&output);
    submit(&mut session, "exit 3");
    session.exp_string("goodbye\r\n").unwrap();
    exited(&mut session, 3);
}

#[test]
fn explicit_exit_after_caught_special_builtin_failure_terminates_repl() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "{ command break foo; exit 5; }");
    exited(&mut session, 5);
}

fn recover_after_error(script: &str, diagnostic: &str) {
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, script);
    session.exp_string(diagnostic).unwrap();
    let rest = session.exp_string("$ ").unwrap();
    assert!(!rest.contains("\r\nunreachable\r\n"));
    submit(&mut session, "echo $?");
    assert_no_output(&session.exp_string("2\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo after");
    assert_no_output(&session.exp_string("after\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn expansion_error_returns_to_prompt() {
    let _pty_test_guard = serialize_pty_test();
    recover_after_error("echo ${nope?missing}; echo unreachable", "missing");
}

#[test]
fn special_builtin_redirection_error_returns_to_prompt() {
    let _pty_test_guard = serialize_pty_test();
    // The session has a private cwd; this parent directory cannot exist.
    recover_after_error(": > absent/dir/x; echo unreachable", "epsh: ");
}

#[test]
fn invalid_break_operand_outside_loop_returns_to_prompt() {
    let _pty_test_guard = serialize_pty_test();
    recover_after_error("break foo; echo unreachable", "break: ");
}

#[test]
fn invalid_break_operand_inside_loop_returns_to_prompt() {
    let _pty_test_guard = serialize_pty_test();
    recover_after_error("while :; do break foo; echo unreachable; done", "break: ");
}

#[test]
fn invalid_continue_operand_inside_loop_returns_to_prompt() {
    let _pty_test_guard = serialize_pty_test();
    recover_after_error(
        "while :; do continue foo; echo unreachable; done",
        "continue: ",
    );
}

#[test]
fn dot_missing_file_returns_to_prompt() {
    let _pty_test_guard = serialize_pty_test();
    recover_after_error(". ./missing-script; echo unreachable", ".: ");
}

#[test]
fn dot_missing_argument_returns_to_prompt() {
    let _pty_test_guard = serialize_pty_test();
    recover_after_error(".; echo unreachable", ".: ");
}

#[test]
fn errexit_still_ends_session_on_false() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "set -e");
    session.exp_string("$ ").unwrap();
    session.send_line("false; echo unreachable").unwrap();
    let output = session.exp_eof().unwrap();
    assert!(!output.contains("\r\nunreachable\r\n"));
    assert!(matches!(
        session.process().wait().unwrap(),
        WaitStatus::Exited(_, 1)
    ));
}

fn history_session(dir: &TempDir, explicit: bool) -> PtySession {
    let path = explicit.then(|| dir.path().join("private-history"));
    history_session_with_file(dir, path.as_deref())
}

fn history_session_with_file(dir: &TempDir, path: Option<&std::path::Path>) -> PtySession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_so1sh"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "dumb")
        .env("SO1SH_HIGHLIGHT", "0")
        .env("HOME", dir.path())
        .current_dir(dir.path());
    if let Some(path) = path {
        command.env("SO1SH_HISTFILE", path);
    }
    let mut session = spawn_pty(command);
    session.process_mut().set_kill_timeout(Some(1000));
    session
}

#[test]
fn history_survives_sessions_and_ctrl_r_searches_it() {
    let _pty_test_guard = serialize_pty_test();
    for explicit in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut first = history_session(&dir, explicit);
        first.exp_string("$ ").unwrap();
        let recalled = "echo persisted-history-marker; echo executed >> executions";
        submit(&mut first, recalled);
        first.exp_string("persisted-history-marker\r\n").unwrap();
        first.exp_string("$ ").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("executions")).unwrap(),
            "executed\n"
        );
        submit(&mut first, "exit");
        exited(&mut first, 0);
        let path = dir.path().join(if explicit {
            "private-history"
        } else {
            ".so1sh_history"
        });
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("echo persisted-history-marker")
        );
        let mut second = history_session(&dir, explicit);
        second.exp_string("$ ").unwrap();
        second.send_control('r').unwrap();
        second.send("persisted-history-marker").unwrap();
        second.send_line("").unwrap();
        second.exp_string(&format!("$ {recalled}")).unwrap();
        // Search acceptance must not execute; the next Enter executes exactly once.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("executions")).unwrap(),
            "executed\n"
        );
        submit(&mut second, "");
        assert_no_output(&second.exp_string("persisted-history-marker\r\n").unwrap());
        second.exp_string("$ ").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("executions")).unwrap(),
            "executed\nexecuted\n"
        );
        submit(&mut second, "exit");
        exited(&mut second, 0);
    }
}

#[test]
fn missing_redirection_target_is_reported_without_continuation() {
    let _pty_test_guard = serialize_pty_test();
    for input in ["echo >", "cat <<"] {
        let (mut session, _dir) = session();
        session.exp_string("$ ").unwrap();
        session.send_line(input).unwrap();
        session.exp_string("syntax error").unwrap();
        session.exp_string("$ ").unwrap();
        submit(&mut session, "echo $?");
        session.exp_string("2\r\n").unwrap();
        session.exp_string("$ ").unwrap();
        submit(&mut session, "echo after");
        assert_no_output(&session.exp_string("after\r\n").unwrap());
        session.exp_string("$ ").unwrap();
        submit(&mut session, "exit");
        exited(&mut session, 0);
    }
}

#[test]
fn empty_heredoc_delimiter_finishes_with_one_enter() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session.send_line("cat <<''").unwrap();
    session.exp_string("> ").unwrap();
    session.send_line("body").unwrap();
    session.exp_string("> ").unwrap();
    // One Enter terminates the empty delimiter line, without a second blank line.
    session.send_line("").unwrap();
    session.exp_string("\r\nbody\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn backslash_newline_uses_continuation_prompt() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session.send_line("echo joined\\").unwrap();
    session.exp_string("> ").unwrap();
    session.send_line("-word").unwrap();
    session.exp_string("\r\njoined-word\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

fn history_error_keeps_session_working(mut session: PtySession) {
    session.exp_string("so1sh: history:").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo after");
    assert_no_output(&session.exp_string("after\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn history_directory_reports_error_and_keeps_session_working() {
    let _pty_test_guard = serialize_pty_test();
    let dir = tempfile::tempdir().unwrap();
    history_error_keeps_session_working(history_session_with_file(&dir, Some(dir.path())));
}

#[test]
fn history_sync_error_keeps_session_working() {
    let _pty_test_guard = serialize_pty_test();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history");
    let mut session = history_session_with_file(&dir, Some(&path));
    session.exp_string("$ ").unwrap();
    // Initialization succeeded; make only the subsequent sync fail.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    submit(&mut session, "echo after");
    session.exp_string("so1sh: history:").unwrap();
    session.exp_string("\r\n").unwrap();
    assert_no_output(&session.exp_string("after\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    // Later submissions (including exit) must remain non-fatal as well.
    submit(&mut session, "exit");
    session.exp_string("so1sh: history:").unwrap();
    exited(&mut session, 0);
}

#[test]
fn history_unwritable_path_reports_error_and_keeps_session_working() {
    let _pty_test_guard = serialize_pty_test();
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("read-only-history");
    std::fs::write(&file, "").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
    // Root bypasses mode bits: an uncreatable descendant is the root-safe oracle.
    let path = if unsafe { libc::geteuid() } == 0 {
        file.join("history")
    } else {
        file
    };
    assert!(
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .is_err()
    );
    history_error_keeps_session_working(history_session_with_file(&dir, Some(&path)));
}

#[test]
fn multiline_if_uses_continuation_prompt() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session.send_line("if true; then").unwrap();
    session.exp_string("> ").unwrap();
    submit(&mut session, "echo yes");
    session.exp_string("> ").unwrap();
    submit(&mut session, "fi");
    session.exp_string("\r\nyes\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn unterminated_quote_continues_on_next_line() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo 'first");
    session.exp_string("> ").unwrap();
    submit(&mut session, "second'");
    session.exp_string("\r\nfirst\r\nsecond\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn trailing_operators_and_heredoc_need_more_input() {
    let _pty_test_guard = serialize_pty_test();
    for (start, lines, output) in [
        ("echo pipeline |", vec!["cat"], "pipeline"),
        ("true &&", vec!["echo and-list"], "and-list"),
        ("cat <<EOF", vec!["heredoc-body", "EOF"], "heredoc-body"),
    ] {
        let (mut session, _dir) = session();
        session.exp_string("$ ").unwrap();
        submit(&mut session, "PS2='more> '");
        session.exp_string("$ ").unwrap();
        submit(&mut session, start);
        for line in lines {
            session.exp_string("more> ").unwrap();
            submit(&mut session, line);
        }
        session.exp_string(&format!("\r\n{output}\r\n")).unwrap();
        session.exp_string("$ ").unwrap();
        submit(&mut session, "exit");
        exited(&mut session, 0);
    }
}

#[test]
fn ctrl_c_discards_prompt_input_without_exiting() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session.send("echo discarded-input").unwrap();
    session.send_control('c').unwrap();
    session.exp_string("\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo alive");
    assert_no_output(&session.exp_string("alive\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

// Always clean up test jobs, including baseline failures before shell recovery.
struct TestJobs(Vec<i32>);
impl Drop for TestJobs {
    fn drop(&mut self) {
        for &pid in &self.0 {
            // SAFETY: only PIDs created by this test's private shell are targeted.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

fn foreground_signal(pipeline: bool, signal: u8, status: i32) {
    let (mut session, dir) = session();
    session.exp_string("$ ").unwrap();
    let command = if pipeline {
        "sh -c 'echo $$ > first; echo first-ready >&2; exec sleep 100' | sh -c 'echo $$ > last; echo last-ready >&2; exec cat'"
    } else {
        "sh -c 'echo $$ > first; echo first-ready >&2; exec sleep 100'"
    };
    submit(&mut session, command);
    if pipeline {
        session.exp_regex("(?:first|last)-ready\\r\\n").unwrap();
        session.exp_regex("(?:first|last)-ready\\r\\n").unwrap();
    } else {
        session.exp_string("first-ready\r\n").unwrap();
    }
    let names = if pipeline {
        vec!["first", "last"]
    } else {
        vec!["first"]
    };
    let mut jobs = TestJobs(
        names
            .iter()
            .map(|name| {
                std::fs::read_to_string(dir.path().join(name))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap()
            })
            .collect(),
    );
    wait_for_process_group(&jobs.0, jobs.0[0]);
    let mut cleanup = (status == 148).then(|| ProcessGroupCleanup::new(jobs.0[0]));
    let tty = session.process().get_file_handle().unwrap();
    use std::os::fd::AsRawFd;
    assert_eq!(unsafe { libc::tcgetpgrp(tty.as_raw_fd()) }, jobs.0[0]);
    session
        .send(std::str::from_utf8(&[signal]).unwrap())
        .unwrap();
    session.flush().unwrap();
    if status == 148 {
        let stopped = session.exp_string("$ ").unwrap();
        let stopped = String::from_utf8(strip_ansi_escapes::strip(stopped)).unwrap();
        assert!(stopped.contains("Stopped"), "{stopped:?}");
    } else {
        session.exp_string("$ ").unwrap();
    }
    submit(&mut session, "echo $?");
    let status_output = session.exp_string(&format!("{status}\r\n")).unwrap();
    if status != 148 {
        assert_no_output(&status_output);
    }
    session.exp_string("$ ").unwrap();
    if let Some(cleanup) = &cleanup {
        for &pid in &jobs.0 {
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                0,
                "stopped job {pid} vanished"
            );
        }
        cleanup.terminate();
        wait_for_job_state(&mut session, "Killed");
    }
    // Interrupted jobs are terminated/reaped, and stopped-job fixtures are
    // explicitly killed and reaped below so tests cannot leak a child.
    for &pid in &jobs.0 {
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "job {pid} is still alive"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    if let Some(cleanup) = &mut cleanup {
        cleanup.disarm();
    }
    jobs.0.clear(); // Never signal an already reaped PID that could be reused.
    let after = if status == 148 {
        // A job notification is asynchronous stderr output; avoid treating its
        // newline as the command editor's submission boundary.
        session.send_line("echo after").unwrap();
        session.exp_string("after\r\n").unwrap()
    } else {
        submit(&mut session, "echo after");
        session.exp_string("after\r\n").unwrap()
    };
    if status != 148 {
        assert_no_output(&after);
    }
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn foreground_ctrl_c_returns_status_130() {
    let _pty_test_guard = serialize_pty_test();
    foreground_signal(false, 0x03, 130);
}
#[test]
fn foreground_ctrl_backslash_returns_status_131() {
    let _pty_test_guard = serialize_pty_test();
    foreground_signal(false, 0x1c, 131);
}
#[test]
fn foreground_ctrl_z_is_reported_and_cleaned_up_by_test() {
    let _pty_test_guard = serialize_pty_test();
    foreground_signal(false, 0x1a, 148);
}

#[test]
fn foreground_ctrl_z_retains_stopped_job_for_jobs() {
    let _pty_test_guard = serialize_pty_test();
    use std::os::fd::AsRawFd;

    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    let tty = session.process().get_file_handle().unwrap();
    let shell_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    submit(&mut session, "sleep 100");
    let deadline = pty_deadline();
    let pgid = loop {
        let pgid = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
        if pgid != shell_group {
            break pgid;
        }
        assert!(std::time::Instant::now() < deadline, "no foreground job");
        std::thread::yield_now();
    };
    let mut cleanup = ProcessGroupCleanup::new(pgid);
    session.send("\x1a").unwrap();
    session.flush().unwrap();
    let stopped = session.exp_string("$ ").unwrap();
    let stopped = String::from_utf8(strip_ansi_escapes::strip(stopped)).unwrap();
    assert!(stopped.contains("[1]+  Stopped  sleep 100"), "{stopped:?}");

    submit(&mut session, "jobs");
    let listing = session.exp_string("$ ").unwrap();
    let listing = String::from_utf8(strip_ansi_escapes::strip(listing)).unwrap();
    assert!(listing.contains("[1]+ Stopped  sleep 100"), "{listing:?}");

    cleanup.terminate();
    wait_for_job_state(&mut session, "Killed");
    cleanup.disarm();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn background_job_is_registered_and_listed_as_running() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "sleep 100 &");
    let launch = session.exp_string("$ ").unwrap();
    let pgid = background_pgid(launch.as_bytes());
    let mut cleanup = ProcessGroupCleanup::new(pgid);
    let launch = String::from_utf8(strip_ansi_escapes::strip(launch)).unwrap();
    assert!(launch.contains(&format!("[1] {pgid}")), "{launch:?}");

    submit(&mut session, "jobs -l");
    let listing = session.exp_string("$ ").unwrap();
    let listing = String::from_utf8(strip_ansi_escapes::strip(listing)).unwrap();
    assert!(
        listing.contains(&format!("[1]+ {pgid} Running  sleep 100")),
        "{listing:?}"
    );

    cleanup.terminate();
    wait_for_job_state(&mut session, "Killed");
    cleanup.disarm();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn finished_background_job_is_reported_done() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "sleep 0.1 &");
    let launch = session.exp_string("$ ").unwrap();
    let mut cleanup = ProcessGroupCleanup::new(background_pgid(launch.as_bytes()));
    let notification = wait_for_job_state(&mut session, "Done");
    assert!(
        notification.contains("[1]+  Done  sleep 0.1 &"),
        "{notification:?}"
    );
    cleanup.disarm();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn wait_job_number_stops_resolving_after_job_notification_prunes_it() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "sh -c 'exit 7' &");
    let launch = session.exp_string("$ ").unwrap();
    let mut cleanup = ProcessGroupCleanup::new(background_pgid(launch.as_bytes()));
    let notification = wait_for_job_state(&mut session, "Done");
    assert!(notification.contains("[1]+  Done"), "{notification:?}");
    cleanup.disarm();

    submit(&mut session, "wait %1; echo $?");
    let output = session.exp_string("127\r\n").unwrap();
    assert_no_output(&output);
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn fg_rejects_a_job_that_finished_before_terminal_handoff() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();

    submit(&mut session, "sleep 0.1 & sleep 0.3; fg %1; echo $?");
    let output =
        String::from_utf8(strip_ansi_escapes::strip(session.exp_string("$ ").unwrap())).unwrap();
    let lines = output.lines().collect::<Vec<_>>();
    assert!(
        lines.contains(&"fg: job is already completed"),
        "{output:?}"
    );
    assert!(lines.contains(&"1"), "{output:?}");
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn wait_dollar_bang_keeps_status_after_done_notification() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "sh -c \"exit 7\" &");
    let mut output = session.exp_string("$ ").unwrap();
    let pgid = background_pgid(output.as_bytes());
    let mut cleanup = ProcessGroupCleanup::new(pgid);
    let deadline = pty_deadline();
    while !output.contains("Exit 7") {
        submit(&mut session, "");
        output = session.exp_string("$ ").unwrap();
        assert!(
            std::time::Instant::now() < deadline,
            "Done notification did not arrive: {output:?}"
        );
    }
    assert!(output.contains("[1]+  Done"), "{output:?}");
    cleanup.disarm();

    submit(&mut session, "wait $!; echo $?");
    let status = session.exp_string("7\r\n").unwrap();
    assert_no_output(&status);
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn jobs_resolves_prefix_job_spec() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "sleep 100 &");
    let launch = session.exp_string("$ ").unwrap();
    let mut cleanup = ProcessGroupCleanup::new(background_pgid(launch.as_bytes()));

    submit(&mut session, "jobs %sleep");
    let listing = session.exp_string("$ ").unwrap();
    let listing = String::from_utf8(strip_ansi_escapes::strip(listing)).unwrap();
    assert!(listing.contains("[1]+ Running  sleep 100"), "{listing:?}");

    cleanup.terminate();
    wait_for_job_state(&mut session, "Killed");
    cleanup.disarm();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}
#[test]
fn pipeline_ctrl_c_terminates_every_stage() {
    let _pty_test_guard = serialize_pty_test();
    foreground_signal(true, 0x03, 130);
}
#[test]
fn pipeline_ctrl_z_is_retained_and_cleaned_up_by_test() {
    let _pty_test_guard = serialize_pty_test();
    foreground_signal(true, 0x1a, 148);
}

#[test]
fn children_see_cooked_modes_and_job_mode_changes_are_restored() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    for command in ["stty -a", "stty -icanon -isig; stty -a"] {
        submit(&mut session, command);
        let output = session.exp_string("$ ").unwrap();
        let plain = String::from_utf8(strip_ansi_escapes::strip(output)).unwrap();
        if command == "stty -a" {
            assert!(plain.contains("icanon"));
            assert!(!plain.contains("-icanon"), "{plain}");
            assert!(!plain.contains("-isig"), "{plain}");
        }
    }
    submit(&mut session, "stty -a");
    let output = session.exp_string("$ ").unwrap();
    assert!(!output.contains("-icanon"), "{output}");
    assert!(!output.contains("-isig"), "{output}");
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn terminal_control_does_not_reopen_redirected_closed_stdin() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(
        &mut session,
        "sh -c 'test -t 0 && echo reopened-tty || echo stdin-closed' 0<&-",
    );
    session.exp_string("stdin-closed\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

fn interrupt_loop(builtin_only: bool, signal: u8, status: i32) {
    use std::os::fd::AsRawFd;
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    let tty = session.process().get_file_handle().unwrap();
    let shell_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    let line = if builtin_only {
        "echo running; while true; do :; done; echo forbidden"
    } else {
        "i=0; while true; do i=$((i+1)); echo running; sleep 0.3; done; echo forbidden"
    };
    submit(&mut session, line);
    session.exp_string("running\r\n").unwrap();
    if !builtin_only {
        let deadline = pty_deadline();
        while unsafe { libc::tcgetpgrp(tty.as_raw_fd()) } == shell_group {
            assert!(std::time::Instant::now() < deadline, "no foreground child");
            std::thread::yield_now();
        }
    }
    session
        .send(std::str::from_utf8(&[signal]).unwrap())
        .unwrap();
    session.flush().unwrap();
    let before_prompt = session.exp_string("$ ").unwrap();
    assert!(!before_prompt.contains("forbidden"), "{before_prompt}");
    assert!(
        !before_prompt.contains("running"),
        "loop continued: {before_prompt}"
    );
    submit(&mut session, "echo $?");
    assert_no_output(&session.exp_string(&format!("{status}\r\n")).unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo alive");
    assert_no_output(&session.exp_string("alive\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn interactive_ctrl_c_aborts_sleep_loop() {
    let _pty_test_guard = serialize_pty_test();
    interrupt_loop(false, 0x03, 130);
}
#[test]
fn interactive_ctrl_quit_aborts_sleep_loop() {
    let _pty_test_guard = serialize_pty_test();
    interrupt_loop(false, 0x1c, 131);
}
#[test]
fn interactive_ctrl_c_aborts_builtin_loop() {
    let _pty_test_guard = serialize_pty_test();
    interrupt_loop(true, 0x03, 130);
}
#[test]
fn interactive_ctrl_quit_aborts_builtin_loop() {
    let _pty_test_guard = serialize_pty_test();
    interrupt_loop(true, 0x1c, 131);
}

#[test]
fn background_start_waits_for_foreground_and_restores_parent_terminal() {
    let _pty_test_guard = serialize_pty_test();
    use std::os::fd::AsRawFd;
    let dir = tempfile::tempdir().unwrap();
    let mut command = Command::new("/bin/sh");
    command
        .arg("-i")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "dumb")
        .env("SO1SH_HIGHLIGHT", "0")
        .env("HOME", dir.path())
        .env("PS1", "parent> ")
        .env("SO1SH_BIN", env!("CARGO_BIN_EXE_so1sh"))
        .env("SO1SH_HISTFILE", dir.path().join("history"))
        .current_dir(dir.path());
    let mut session = spawn_pty(command);
    session.process_mut().set_kill_timeout(Some(1000));
    session.exp_string("parent> ").unwrap();
    let tty = session.process().get_file_handle().unwrap();
    let parent_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    session
        .send_line(
            "PS1='$ ' \"$SO1SH_BIN\" & child=$!; echo $child > child; wait $child; echo waited:$?",
        )
        .unwrap();
    let deadline = pty_deadline();
    while !std::fs::read_to_string(dir.path().join("child"))
        .is_ok_and(|raw| raw.trim().parse::<i32>().is_ok())
    {
        assert!(
            std::time::Instant::now() < deadline,
            "parent never started child"
        );
        std::thread::yield_now();
    }
    let pid = std::fs::read_to_string(dir.path().join("child"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut child = TestJobs(vec![pid]);
    // A background child must stop on SIGTTIN instead of printing its own prompt.
    session.exp_string("waited:").unwrap();
    session
        .exp_string(&format!("{}\r\n", 128 + libc::SIGTTIN))
        .unwrap();
    session.exp_string("parent> ").unwrap();
    assert_eq!(unsafe { libc::tcgetpgrp(tty.as_raw_fd()) }, parent_group);
    session.send_line("fg").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo inside");
    session.exp_string("inside\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    session.exp_string("parent> ").unwrap();
    assert_eq!(unsafe { libc::tcgetpgrp(tty.as_raw_fd()) }, parent_group);
    child.0.clear();
    session.send_line("exit").unwrap();
    exited(&mut session, 0);
}

#[test]
fn missing_controlling_terminal_reports_once_and_keeps_repl_working() {
    let _pty_test_guard = serialize_pty_test();
    use std::os::unix::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_so1sh"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "dumb")
        .env("SO1SH_HIGHLIGHT", "0")
        .env("HOME", dir.path())
        .current_dir(dir.path());
    // SAFETY: child-only async-signal-safe detachment; keep stdin's tty fd open.
    unsafe {
        command.pre_exec(|| {
            let mut ignore: libc::sigaction = std::mem::zeroed();
            let mut old = std::mem::zeroed();
            ignore.sa_sigaction = libc::SIG_IGN;
            libc::sigemptyset(&mut ignore.sa_mask);
            if libc::sigaction(libc::SIGHUP, &ignore, &mut old) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let result = libc::ioctl(0, libc::TIOCNOTTY);
            libc::sigaction(libc::SIGHUP, &old, std::ptr::null_mut());
            if result == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut session = spawn_pty(command);
    session.process_mut().set_kill_timeout(Some(1000));
    session.exp_string("so1sh: terminal:").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo after");
    assert_no_output(&session.exp_string("after\r\n").unwrap());
    let output = session.exp_string("$ ").unwrap();
    assert!(!output.contains("terminal:"), "{output}");
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn background_startup_saves_modes_after_foreground_permission() {
    let _pty_test_guard = serialize_pty_test();
    let dir = tempfile::tempdir().unwrap();
    let mut command = Command::new("/bin/sh");
    command
        .arg("-i")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "dumb")
        .env("HOME", dir.path())
        .env("PS1", "parent> ")
        .env("SO1SH_BIN", env!("CARGO_BIN_EXE_so1sh"))
        .current_dir(dir.path());
    let mut session = spawn_pty(command);
    session.process_mut().set_kill_timeout(Some(1000));
    session.exp_string("parent> ").unwrap();
    session.send_line("stty -icanon; PS1='$ ' \"$SO1SH_BIN\" & child=$!; echo $child > child; wait $child; stty icanon; echo ready-for-fg").unwrap();
    let deadline = pty_deadline();
    let pid = loop {
        if let Ok(raw) = std::fs::read_to_string(dir.path().join("child"))
            && let Ok(pid) = raw.trim().parse::<i32>()
        {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline, "job did not start");
        std::thread::yield_now();
    };
    let mut child = TestJobs(vec![pid]);
    session.exp_string("ready-for-fg\r\n").unwrap();
    session.exp_string("parent> ").unwrap();
    // The parent changed modes while so1sh was stopped in its acquisition loop.
    session.send_line("fg").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "stty -a");
    let output = session.exp_string("$ ").unwrap();
    let output = String::from_utf8(strip_ansi_escapes::strip(output)).unwrap();
    assert!(
        output
            .split(|c: char| c.is_whitespace() || c == ';')
            .any(|flag| flag == "icanon"),
        "child inherited stale background modes: {output}"
    );
    submit(&mut session, "exit");
    session.exp_string("parent> ").unwrap();
    child.0.clear();
    session.send_line("exit").unwrap();
    exited(&mut session, 0);
}

#[cfg(target_os = "linux")]
#[test]
fn orphaned_background_startup_falls_back_without_spinning() {
    let _pty_test_guard = serialize_pty_test();
    use std::os::fd::AsRawFd;
    const NAME: &str = "orphaned_background_startup_falls_back_without_spinning";
    if std::env::var("SO1SH_ORPHAN_TEST").as_deref() != Ok(NAME) {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env("SO1SH_ORPHAN_TEST", NAME)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{output:?}");
        assert!(stdout.lines().any(|line| line == "running 1 test"));
        assert!(stdout.contains("test result: ok. 1 passed; 0 failed;"));
        return;
    }
    // Isolated Linux subreaper: reap the deliberately orphaned job, not PID 1.
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    struct Orphan(i32);
    impl Drop for Orphan {
        fn drop(&mut self) {
            if self.0 > 0 {
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                    libc::waitpid(self.0, std::ptr::null_mut(), 0);
                }
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("rc"),
        "/bin/sh -c 'owner=$(/bin/ps -o tpgid= -p $$); test \"$owner\" -eq \"$SO1SH_PARENT_PGID\"' || exit 9\nexit 3\n",
    )
    .unwrap();
    let mut command = Command::new("/bin/sh");
    command
        .arg("-i")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "dumb")
        .env("HOME", dir.path())
        .env("PS1", "parent> ")
        .env("SO1SH_BIN", env!("CARGO_BIN_EXE_so1sh"))
        .env("SO1SH_RC", dir.path().join("rc"))
        .current_dir(dir.path());
    let mut session = spawn_pty(command);
    session.process_mut().set_kill_timeout(Some(1000));
    session.exp_string("parent> ").unwrap();
    let tty = session.process().get_file_handle().unwrap();
    let owner = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    session
        .send_line(&format!("export SO1SH_PARENT_PGID={owner}"))
        .unwrap();
    session.exp_string("parent> ").unwrap();
    let started = std::time::Instant::now();
    // The subshell exits before the background job starts so1sh. Its new parent
    // (this subreaper) belongs to another session, making its group orphaned.
    session.send_line("( /bin/sh -c 'while [ ! -e ready ]; do /bin/sleep 0.01; done; exec \"$SO1SH_BIN\"' </dev/tty >/dev/tty 2>&1 & echo $! > child ); : > ready").unwrap();
    let deadline = started + pty_test_timeout();
    let pid = loop {
        if let Ok(raw) = std::fs::read_to_string(dir.path().join("child"))
            && let Ok(pid) = raw.trim().parse::<i32>()
        {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline, "job did not start");
        std::thread::yield_now();
    };
    let mut orphan = Orphan(pid);
    assert_ne!(unsafe { libc::getpgid(pid) }, owner);
    if let Err(error) = session.exp_string("so1sh: terminal:") {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::kill(pid, libc::SIGKILL) };
        assert_eq!(
            unsafe { libc::wait4(pid, std::ptr::null_mut(), 0, &mut usage) },
            pid
        );
        orphan.0 = 0;
        let cpu_micros = (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) * 1_000_000
            + usage.ru_utime.tv_usec
            + usage.ru_stime.tv_usec;
        panic!("orphan startup used {cpu_micros}us CPU without fallback: {error:?}");
    }
    session
        .exp_string("continuing without terminal job control\r\n")
        .unwrap();
    assert_eq!(unsafe { libc::tcgetpgrp(tty.as_raw_fd()) }, owner);
    let mut status = 0;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::wait4(pid, &mut status, 0, &mut usage) }, pid);
    orphan.0 = 0;
    assert!(libc::WIFEXITED(status));
    assert_eq!(
        libc::WEXITSTATUS(status),
        3,
        "rc failed after fallback (9 means command stole tty)"
    );
    assert!(started.elapsed() < std::time::Duration::from_millis(2500));
    let cpu_micros = (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) * 1_000_000
        + usage.ru_utime.tv_usec
        + usage.ru_stime.tv_usec;
    assert!(cpu_micros < 250_000, "startup consumed {cpu_micros}us CPU");
    session.send_line("exit").unwrap();
    exited(&mut session, 0);
}

mod startup_rc {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    fn session(dir: &TempDir, rc: Option<&OsStr>, home: Option<&OsStr>) -> PtySession {
        let mut command = Command::new(env!("CARGO_BIN_EXE_so1sh"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "dumb")
            .env("SO1SH_SHELL_INTEGRATION", "0")
            .env("SO1SH_HIGHLIGHT", "0")
            .env("SO1SH_HISTFILE", dir.path().join("history"))
            .current_dir(dir.path());
        if let Some(rc) = rc {
            command.env("SO1SH_RC", rc);
        }
        if let Some(home) = home {
            command.env("HOME", home);
        }
        let mut session = spawn_pty(command);
        session.process_mut().set_kill_timeout(Some(1000));
        session
    }

    #[test]
    fn default_file_configures_prompts_variables_and_cwd() {
        let _pty_test_guard = serialize_pty_test();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(
            dir.path().join(".so1shrc"),
            "PS1='rc> '; PS2='rc-more> '; x=5; cd sub\n",
        )
        .unwrap();
        // An empty override falls back to the default file.
        let mut session = session(&dir, Some(OsStr::new("")), Some(dir.path().as_os_str()));
        session.exp_string("rc> ").unwrap();
        submit(&mut session, "echo $x; pwd");
        assert_no_output(&session.exp_string("5\r\n").unwrap());
        session
            .exp_string(&format!("{}/sub\r\n", dir.path().display()))
            .unwrap();
        session.exp_string("rc> ").unwrap();
        session.send_line("if true; then").unwrap();
        session.exp_string("rc-more> ").unwrap();
        session.send_line("echo continued").unwrap();
        session.exp_string("rc-more> ").unwrap();
        submit(&mut session, "fi");
        session.exp_string("\r\ncontinued\r\n").unwrap();
        session.exp_string("rc> ").unwrap();
        submit(&mut session, "exit");
        exited(&mut session, 0);
    }

    #[test]
    fn explicit_file_preserves_path_bytes_and_defines_function() {
        let _pty_test_guard = serialize_pty_test();
        let dir = tempfile::tempdir().unwrap();
        let rc = dir
            .path()
            .join(OsStr::from_bytes(b"rc '; echo injected; # \xff"));
        std::fs::write(&rc, "greet() { echo from-rc; }\n").unwrap();
        // The explicit file overrides the default, even with shell metacharacters.
        std::fs::write(dir.path().join(".so1shrc"), "echo wrong-file; exit 9\n").unwrap();
        let mut session = session(&dir, Some(rc.as_os_str()), Some(dir.path().as_os_str()));
        assert_no_output(&session.exp_string("$ ").unwrap());
        submit(&mut session, "greet");
        assert_no_output(&session.exp_string("from-rc\r\n").unwrap());
        session.exp_string("$ ").unwrap();
        submit(&mut session, "exit");
        exited(&mut session, 0);
    }

    #[test]
    fn relative_rc_without_slash_is_loaded_from_current_directory() {
        let _pty_test_guard = serialize_pty_test();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rcfile"), "PS1='cwd> '; echo cwd-rc\n").unwrap();
        let path_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            path_dir.path().join("rcfile"),
            "PS1='path> '; echo path-rc\n",
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_so1sh"));
        command
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", path_dir.path().display()),
            )
            .env("TERM", "dumb")
            .env("SO1SH_HIGHLIGHT", "0")
            .env("HOME", dir.path())
            .env("SO1SH_RC", "rcfile")
            .current_dir(&dir);
        let mut session = spawn_pty(command);
        session.process_mut().set_kill_timeout(Some(1000));
        session.exp_string("cwd-rc\r\n").unwrap();
        session.exp_regex("cwd> ").unwrap();
        submit(&mut session, "exit");
        exited(&mut session, 0);
    }

    fn error_then_prompt(mut session: PtySession, diagnostic: &str) {
        session.exp_string(diagnostic).unwrap();
        session.exp_string("$ ").unwrap();
        submit(&mut session, "echo $?");
        assert_no_output(&session.exp_string("2\r\n").unwrap());
        session.exp_string("$ ").unwrap();
        submit(&mut session, "echo after");
        assert_no_output(&session.exp_string("after\r\n").unwrap());
        session.exp_string("$ ").unwrap();
        submit(&mut session, "exit");
        exited(&mut session, 0);
    }

    fn rc_error(source: &str, diagnostic: &str) {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join("rc");
        std::fs::write(&rc, source).unwrap();
        error_then_prompt(session(&dir, Some(rc.as_os_str()), None), diagnostic);
    }

    #[test]
    fn syntax_error_keeps_session_working() {
        let _pty_test_guard = serialize_pty_test();
        rc_error("if true; then\n", "syntax error");
    }

    #[test]
    fn runtime_error_keeps_session_working() {
        let _pty_test_guard = serialize_pty_test();
        rc_error("echo ${nope?rc-missing}; echo forbidden\n", "rc-missing");
    }

    #[test]
    fn missing_explicit_file_reports_error_without_default_fallback() {
        let _pty_test_guard = serialize_pty_test();
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join("missing-rc");
        std::fs::write(dir.path().join(".so1shrc"), "exit 9\n").unwrap();
        error_then_prompt(
            session(&dir, Some(rc.as_os_str()), Some(dir.path().as_os_str())),
            ".:",
        );
    }

    #[test]
    fn exit_in_file_ends_session_before_first_prompt() {
        let _pty_test_guard = serialize_pty_test();
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join("rc");
        std::fs::write(&rc, "exit 3\n").unwrap();
        let mut session = session(&dir, Some(rc.as_os_str()), None);
        exited(&mut session, 3);
    }

    #[test]
    fn return_in_file_keeps_session_and_skips_rest_of_file() {
        let _pty_test_guard = serialize_pty_test();
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join("rc");
        std::fs::write(&rc, "x=before; return 7; x=forbidden\n").unwrap();
        let mut session = session(&dir, Some(rc.as_os_str()), None);
        assert_no_output(&session.exp_string("$ ").unwrap());
        submit(&mut session, "echo $? $x");
        assert_no_output(&session.exp_string("7 before\r\n").unwrap());
        session.exp_string("$ ").unwrap();
        submit(&mut session, "exit");
        exited(&mut session, 0);
    }

    #[test]
    fn missing_default_and_unset_or_empty_home_are_silent() {
        let _pty_test_guard = serialize_pty_test();
        let dir = tempfile::tempdir().unwrap();
        let mut absent = session(&dir, None, Some(dir.path().as_os_str()));
        assert_no_output(&absent.exp_string("$ ").unwrap());
        submit(&mut absent, "exit");
        exited(&mut absent, 0);
        // A local dotfile must not be sourced through a relative empty HOME.
        std::fs::write(dir.path().join(".so1shrc"), "exit 9\n").unwrap();
        for home in [None, Some(OsStr::new(""))] {
            let mut session = session(&dir, Some(OsStr::new("")), home);
            assert_no_output(&session.exp_string("$ ").unwrap());
            submit(&mut session, "exit");
            exited(&mut session, 0);
        }
    }

    #[test]
    fn foreground_without_jobs_returns_failure_status() {
        let _pty_test_guard = serialize_pty_test();
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(&dir, None, Some(dir.path().as_os_str()));
        assert_no_output(&session.exp_string("$ ").unwrap());
        submit(&mut session, "fg; echo $?");
        session.exp_string("fg: no current job\r\n").unwrap();
        assert_no_output(&session.exp_string("1\r\n").unwrap());
        session.exp_string("$ ").unwrap();
        submit(&mut session, "exit");
        exited(&mut session, 0);
    }
}

#[test]
fn stopped_job_can_be_foregrounded_and_interrupted() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    use std::os::fd::AsRawFd;
    let tty = session.process().get_file_handle().unwrap();
    let shell_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    submit(&mut session, "sleep 100");
    let deadline = pty_deadline();
    let pgid = loop {
        let group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
        if group != shell_group {
            break group;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "foreground sleep did not start"
        );
        std::thread::yield_now();
    };
    let mut cleanup = ProcessGroupCleanup::new(pgid);
    session.send("\x1a").unwrap();
    session.flush().unwrap();
    let stopped = session.exp_string("$ ").unwrap();
    assert!(
        String::from_utf8(strip_ansi_escapes::strip(stopped))
            .unwrap()
            .contains("Stopped")
    );
    submit(&mut session, "fg");
    let deadline = pty_deadline();
    while unsafe { libc::tcgetpgrp(tty.as_raw_fd()) } != pgid {
        assert!(
            std::time::Instant::now() < deadline,
            "fg did not transfer terminal ownership to {pgid}"
        );
        std::thread::yield_now();
    }
    assert!(unsafe { libc::tcgetpgrp(tty.as_raw_fd()) } == pgid);
    session.send("\x03").unwrap();
    session.flush().unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo $?");
    assert_no_output(&session.exp_string("130\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    assert_eq!(unsafe { libc::kill(pgid, 0) }, -1);
    cleanup.disarm();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn stopped_job_can_be_backgrounded_then_waited_by_job_spec() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    use std::os::fd::AsRawFd;
    let tty = session.process().get_file_handle().unwrap();
    let shell_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    submit(&mut session, "sh -c 'sleep 1.5'");
    let deadline = pty_deadline();
    while unsafe { libc::tcgetpgrp(tty.as_raw_fd()) } == shell_group {
        assert!(
            std::time::Instant::now() < deadline,
            "foreground child did not start"
        );
        std::thread::yield_now();
    }
    session.send("\x1a").unwrap();
    session.flush().unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "bg");
    session.exp_string("[1]+ sh -c 'sleep 1.5' &\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "jobs");
    let listing = session.exp_string("$ ").unwrap();
    let listing = String::from_utf8(strip_ansi_escapes::strip(listing)).unwrap();
    assert!(listing.contains("Running"), "{listing:?}");
    submit(&mut session, "wait %1; echo $?");
    assert_no_output(&session.exp_string("0\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn interactive_wait_preserves_background_exit_statuses() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "sleep 0.2 & wait $!; echo $?");
    let output = session.exp_string("0\r\n").unwrap();
    let output = String::from_utf8(strip_ansi_escapes::strip(output)).unwrap();
    assert!(output.contains("[1] "), "launch notice missing: {output:?}");
    session.exp_string("$ ").unwrap();
    submit(&mut session, "sh -c 'exit 7' & wait $!; echo $?");
    let output = session.exp_string("7\r\n").unwrap();
    let output = String::from_utf8(strip_ansi_escapes::strip(output)).unwrap();
    assert!(output.contains("[2] "), "launch notice missing: {output:?}");
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn backgrounded_so1sh_can_acquire_tty_after_sigttin_timeout_window() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, &format!("{} &", env!("CARGO_BIN_EXE_so1sh")));
    let launch = session.exp_string("$ ").unwrap();
    let pgid = background_pgid(launch.as_bytes());
    let mut cleanup = ProcessGroupCleanup::new(pgid);
    let deadline = pty_deadline();
    loop {
        submit(&mut session, "jobs");
        let listing = session.exp_string("$ ").unwrap();
        let listing = String::from_utf8(strip_ansi_escapes::strip(listing)).unwrap();
        if listing.contains("Stopped") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "nested so1sh did not stop for SIGTTIN: {listing:?}"
        );
        std::thread::yield_now();
    }
    // Keep the nested shell stopped beyond its one-second active wait budget
    // while synchronizing on observable prompt/job-table round trips.
    let resumed_at = std::time::Instant::now() + std::time::Duration::from_millis(1100);
    while std::time::Instant::now() < resumed_at {
        submit(&mut session, "jobs");
        session.exp_string("$ ").unwrap();
        std::thread::yield_now();
    }
    submit(&mut session, "bg %1");
    session.exp_string("$ ").unwrap();
    let listing = wait_for_job_state(&mut session, "Running");
    assert!(listing.contains("Running"), "{listing:?}");
    cleanup.terminate();
    wait_for_job_state(&mut session, "Killed");
    cleanup.disarm();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn background_launch_notice_precedes_later_commands_on_same_line() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "sleep 100 & echo tail");
    let output = session.exp_string("$ ").unwrap();
    let output = String::from_utf8(strip_ansi_escapes::strip(output)).unwrap();
    let pgid = background_pgid(output.as_bytes());
    let cleanup = ProcessGroupCleanup::new(pgid);
    let launch = output.find(&format!("[1] {pgid}")).unwrap();
    let tail = output.find("tail").unwrap();
    assert!(
        launch < tail,
        "launch notice came after later command: {output:?}"
    );
    cleanup.terminate();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn background_done_notification_includes_nonzero_exit_and_is_removed() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "sh -c 'exit 3' &");
    let launch = session.exp_string("$ ").unwrap();
    let pgid = background_pgid(launch.as_bytes());
    let mut cleanup = ProcessGroupCleanup::new(pgid);

    let deadline = pty_deadline();
    let mut observed = String::new();
    while !observed.contains("Done") {
        submit(&mut session, "jobs");
        let output = session.exp_string("$ ").unwrap();
        observed.push_str(&String::from_utf8(strip_ansi_escapes::strip(output)).unwrap());
        assert!(
            std::time::Instant::now() < deadline,
            "completion notification not reported: {observed:?}"
        );
    }
    assert!(
        observed.contains("[1]+  Done  (Exit 3) sh -c 'exit 3'"),
        "{observed:?}"
    );
    submit(&mut session, "jobs");
    let after_report = session.exp_string("$ ").unwrap();
    let after_report = String::from_utf8(strip_ansi_escapes::strip(after_report)).unwrap();
    assert!(
        !after_report.contains("exit 3"),
        "reported job remained listed: {after_report:?}"
    );
    cleanup.disarm();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn killed_background_job_notifies_before_removal() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "sleep 100 &");
    let launch = session.exp_string("$ ").unwrap();
    let pgid = background_pgid(launch.as_bytes());
    let cleanup = ProcessGroupCleanup::new(pgid);
    cleanup.terminate();
    let deadline = pty_deadline();
    let mut notification = String::new();
    while !notification.contains("Killed") {
        submit(&mut session, "jobs");
        notification =
            String::from_utf8(strip_ansi_escapes::strip(session.exp_string("$ ").unwrap()))
                .unwrap();
        assert!(
            std::time::Instant::now() < deadline,
            "kill notification was not reported"
        );
    }
    assert!(
        notification.contains("[1]+  Killed  sleep 100"),
        "{notification:?}"
    );
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn ctrl_z_reports_only_the_shell_job_notification() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    use std::os::fd::AsRawFd;
    let tty = session.process().get_file_handle().unwrap();
    let shell_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    submit(&mut session, "sleep 100");
    let deadline = pty_deadline();
    let pgid = loop {
        let group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
        if group != shell_group {
            break group;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sleep did not become foreground"
        );
        std::thread::yield_now();
    };
    let cleanup = ProcessGroupCleanup::new(pgid);
    session.send("\x1a").unwrap();
    session.flush().unwrap();
    let output = session.exp_string("$ ").unwrap();
    let output = String::from_utf8(strip_ansi_escapes::strip(output)).unwrap();
    assert!(
        !output.contains("epsh: stopped"),
        "internal diagnostic leaked: {output:?}"
    );
    assert!(output.contains("[1]+  Stopped  sleep 100"), "{output:?}");
    cleanup.terminate();
    let deadline = pty_deadline();
    loop {
        submit(&mut session, "jobs");
        let output = session.exp_string("$ ").unwrap();
        if String::from_utf8(strip_ansi_escapes::strip(output))
            .unwrap()
            .contains("Killed")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "cleanup kill was not reaped"
        );
    }
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn first_exit_with_stopped_jobs_warns_and_second_exits() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    use std::os::fd::AsRawFd;
    let tty = session.process().get_file_handle().unwrap();
    let shell_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    submit(&mut session, "sleep 100");
    let deadline = pty_deadline();
    let pgid = loop {
        let group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
        if group != shell_group {
            break group;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sleep did not become foreground"
        );
        std::thread::yield_now();
    };
    let mut cleanup = ProcessGroupCleanup::new(pgid);
    session.send("\x1a").unwrap();
    session.flush().unwrap();
    session.exp_string("$ ").unwrap();

    submit(&mut session, "exit");
    session.exp_string("There are stopped jobs.\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 148);
    let deadline = pty_deadline();
    while unsafe { libc::kill(pgid, 0) } == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "stopped job survived confirmed exit"
        );
        std::thread::yield_now();
    }
    cleanup.disarm();
}

#[test]
fn ctrl_d_with_stopped_jobs_requires_confirmation() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    use std::os::fd::AsRawFd;
    let tty = session.process().get_file_handle().unwrap();
    let shell_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    submit(&mut session, "sleep 100");
    let deadline = pty_deadline();
    let pgid = loop {
        let group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
        if group != shell_group {
            break group;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sleep did not become foreground"
        );
        std::thread::yield_now();
    };
    let mut cleanup = ProcessGroupCleanup::new(pgid);
    session.send("\x1a").unwrap();
    session.flush().unwrap();
    session.exp_string("$ ").unwrap();

    session.send_control('d').unwrap();
    session.exp_string("There are stopped jobs.\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    session.send_control('d').unwrap();
    exited(&mut session, 148);
    let deadline = pty_deadline();
    while unsafe { libc::kill(pgid, 0) } == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "stopped job survived confirmed EOF"
        );
        std::thread::yield_now();
    }
    cleanup.disarm();
}

#[test]
fn background_launch_notice_bypasses_compound_stdout_redirection() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, dir) = session();
    session.exp_string("$ ").unwrap();
    let output_file = dir.path().join("launch-output");
    submit(
        &mut session,
        &format!("{{ sleep 100 & }} > {}", output_file.display()),
    );
    let output = session.exp_string("$ ").unwrap();
    let file_output = std::fs::read(&output_file).unwrap();
    let pgid = background_pgid(&[output.as_bytes(), &file_output].concat());
    let mut cleanup = ProcessGroupCleanup::new(pgid);
    assert!(
        file_output.is_empty(),
        "job-control notice leaked into redirected stdout: {file_output:?}"
    );
    assert!(
        String::from_utf8(strip_ansi_escapes::strip(output.clone()))
            .unwrap()
            .contains(&format!("[1] {pgid}")),
        "launch notice was not written to the terminal: {output:?}"
    );
    cleanup.terminate();
    wait_for_job_state(&mut session, "Killed");
    cleanup.disarm();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn foregrounded_job_stopped_again_is_reported_stopped() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    use std::os::fd::AsRawFd;
    let tty = session.process().get_file_handle().unwrap();
    let shell_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    submit(&mut session, "sleep 100");
    let deadline = pty_deadline();
    let pgid = loop {
        let group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
        if group != shell_group {
            break group;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sleep did not become foreground"
        );
        std::thread::yield_now();
    };
    let mut cleanup = ProcessGroupCleanup::new(pgid);
    session.send("\x1a").unwrap();
    session.flush().unwrap();
    let first_stop = session.exp_string("$ ").unwrap();
    assert!(
        first_stop.contains("[1]+  Stopped  sleep 100"),
        "{first_stop:?}"
    );

    submit(&mut session, "fg");
    let deadline = pty_deadline();
    while unsafe { libc::tcgetpgrp(tty.as_raw_fd()) } != pgid {
        assert!(
            std::time::Instant::now() < deadline,
            "fg did not foreground {pgid}"
        );
        std::thread::yield_now();
    }
    session.send("\x1a").unwrap();
    session.flush().unwrap();
    let second_stop = session.exp_string("$ ").unwrap();
    assert!(
        second_stop.contains("[1]+  Stopped  sleep 100"),
        "second stop was not reported: {second_stop:?}"
    );
    cleanup.terminate();
    wait_for_job_state(&mut session, "Killed");
    cleanup.disarm();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn stopped_job_exit_warning_resets_after_a_non_exit_command() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    use std::os::fd::AsRawFd;
    let tty = session.process().get_file_handle().unwrap();
    let shell_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    submit(&mut session, "sleep 100");
    let deadline = pty_deadline();
    let pgid = loop {
        let group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
        if group != shell_group {
            break group;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sleep did not become foreground"
        );
        std::thread::yield_now();
    };
    let mut cleanup = ProcessGroupCleanup::new(pgid);
    session.send("\x1a").unwrap();
    session.flush().unwrap();
    session.exp_string("$ ").unwrap();

    submit(&mut session, "exit 7");
    session.exp_string("There are stopped jobs.\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo still-here");
    session.exp_string("still-here\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit 7");
    session.exp_string("There are stopped jobs.\r\n").unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit 7");
    exited(&mut session, 7);
    let deadline = pty_deadline();
    while unsafe { libc::kill(pgid, 0) } == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "stopped job survived confirmed exit"
        );
        std::thread::yield_now();
    }
    cleanup.disarm();
}

#[test]
fn shell_integration_starts_primary_prompt_with_osc133() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session_with_shell_integration(None);
    let prompt = session.exp_string("$ ").unwrap();
    assert!(
        prompt
            .as_bytes()
            .windows(b"\x1b]133;A\x1b\\".len())
            .any(|window| window == b"\x1b]133;A\x1b\\"),
        "primary prompt is missing OSC 133;A: {prompt:?}"
    );
    session.send_line("exit").unwrap();
    session.exp_eof().unwrap();
    assert!(matches!(
        session.process().wait().unwrap(),
        WaitStatus::Exited(_, 0)
    ));
}

fn submit_until_sequence(session: &mut PtySession, line: &str, end: &str) -> String {
    session.send_line(line).unwrap();
    let accepted = session.exp_string("\r\n").unwrap();
    let (output, matched) = session
        .exp_any(vec![ReadUntil::String(end.to_owned())])
        .unwrap();
    assert_eq!(matched, end);
    format!("{accepted}{output}{matched}")
}

#[test]
fn shell_integration_marks_echo_false_and_empty_lines() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session_with_shell_integration(None);
    let prompt = capture_primary_prompt(&mut session);
    assert_sequences_in_order(&prompt, &["\x1b]7;file://", OSC133_A, OSC133_B]);

    let echo = submit_until_sequence(&mut session, "echo hi", OSC133_D0);
    assert_sequences_in_order(&echo, &[OSC133_C, "hi\r\n", OSC133_D0]);
    capture_primary_prompt(&mut session);

    let redirected = submit_until_sequence(&mut session, "echo file-only > output", OSC133_D0);
    assert_sequences_in_order(&redirected, &[OSC133_C, OSC133_D0]);
    assert_eq!(
        std::fs::read_to_string(_dir.path().join("output")).unwrap(),
        "file-only\n"
    );
    capture_primary_prompt(&mut session);

    let false_line = submit_until_sequence(&mut session, "false", OSC133_D1);
    assert_sequences_in_order(&false_line, &[OSC133_C, OSC133_D1]);
    capture_primary_prompt(&mut session);

    let empty_line = submit_until_sequence(&mut session, "", OSC133_D1);
    assert_sequences_in_order(&empty_line, &[OSC133_C, OSC133_D1]);
    capture_primary_prompt(&mut session);
    submit_until_sequence(&mut session, "exit", OSC133_D1);
    exited(&mut session, 1);
}

#[test]
fn shell_integration_reports_cwd_after_cd() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session_with_shell_integration(None);
    capture_primary_prompt(&mut session);
    let cd = submit_until_sequence(&mut session, "cd /tmp", OSC133_D0);
    assert_sequences_in_order(&cd, &[OSC133_C, OSC133_D0]);
    let prompt = capture_primary_prompt(&mut session);
    assert!(prompt.contains("\x1b]7;file://"), "{prompt:?}");
    let expected_url = format!("\x1b]7;file://{}/tmp\x1b\\", encoded_local_hostname());
    assert!(
        prompt.contains(&expected_url),
        "OSC 7 did not report the system hostname and /tmp ({expected_url:?}): {prompt:?}"
    );
    assert_sequences_in_order(&prompt, &["/tmp\x1b\\", OSC133_A, OSC133_B]);
    submit_until_sequence(&mut session, "exit", OSC133_D0);
    exited(&mut session, 0);
}

#[test]
fn shell_integration_multiline_input_has_one_prompt_and_command_block() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session_with_shell_integration(None);
    let prompt = capture_primary_prompt(&mut session);
    assert_eq!(prompt.matches(OSC133_A).count(), 1);
    assert_eq!(prompt.matches(OSC133_B).count(), 1);

    session.send_line("if true; then").unwrap();
    let first_continuation = session.exp_string("> ").unwrap();
    assert!(!first_continuation.contains(OSC133_A));
    assert!(!first_continuation.contains(OSC133_B));
    session.send_line("echo multiline-ok").unwrap();
    let second_continuation = session.exp_string("> ").unwrap();
    assert!(!second_continuation.contains(OSC133_A));
    assert!(!second_continuation.contains(OSC133_B));
    let output = submit_until_sequence(&mut session, "fi", "\x1b]133;D;0\x1b\\");
    assert_sequences_in_order(&output, &[OSC133_C, "multiline-ok\r\n", OSC133_D0]);
    assert_eq!(output.matches(OSC133_C).count(), 1);
    assert_eq!(output.matches(OSC133_D0).count(), 1);
    capture_primary_prompt(&mut session);
    submit_until_sequence(&mut session, "exit", OSC133_D0);
    exited(&mut session, 0);
}

#[test]
fn shell_integration_ctrl_c_discards_input_without_command_marks() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session_with_shell_integration(None);
    capture_primary_prompt(&mut session);
    session.send("echo should-not-run").unwrap();
    session.flush().unwrap();
    session.send_control('c').unwrap();
    let cancelled = session.exp_string("$ ").unwrap();
    assert!(!cancelled.contains(OSC133_C), "{cancelled:?}");
    assert!(!cancelled.contains("\x1b]133;D;"), "{cancelled:?}");
    expect_sequence(&mut session, OSC133_B);
    submit_until_sequence(&mut session, "exit", OSC133_D0);
    exited(&mut session, 0);
}

#[test]
fn shell_integration_can_be_disabled() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session_with_shell_integration(Some("0"));
    let first = session.exp_string("$ ").unwrap();
    submit(&mut session, "echo integration-disabled");
    let output = session.exp_string("integration-disabled\r\n").unwrap();
    let next = session.exp_string("$ ").unwrap();
    session.send_line("exit").unwrap();
    let tail = session.exp_eof().unwrap();
    let combined = format!("{first}{output}{next}{tail}");
    for marker in ["\x1b]7;", "\x1b]133;"] {
        assert!(
            !combined.contains(marker),
            "opt-out leaked {marker:?}: {combined:?}"
        );
    }
    assert!(matches!(
        session.process().wait().unwrap(),
        WaitStatus::Exited(_, 0)
    ));
}

#[test]
fn shell_integration_prompt_markers_do_not_break_wrapped_input() {
    use std::os::fd::AsRawFd;

    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session_with_shell_integration(None);
    let tty = session.process().get_file_handle().unwrap();
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 18,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCSWINSZ, &size) },
        0
    );
    capture_primary_prompt(&mut session);
    let output = submit_until_sequence(&mut session, "printf '%s\\n' wrapped-input-ok", OSC133_D0);
    assert_sequences_in_order(&output, &[OSC133_C, "wrapped-input-ok\r\n", OSC133_D0]);
    capture_primary_prompt(&mut session);
    submit_until_sequence(&mut session, "exit", OSC133_D0);
    exited(&mut session, 0);
}

#[test]
fn tab_completes_a_unique_builtin_command() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, _dir) = session();

    session.exp_string("$ ").unwrap();
    session.send("ech").unwrap();
    session.send("\t").unwrap();
    session.send("tab-completed").unwrap();
    session.flush().unwrap();
    session.send_line("").unwrap();
    session.exp_string("\r\n").unwrap();
    assert_no_output(&session.exp_string("tab-completed\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn tab_lists_multiple_file_matches() {
    let _pty_test_guard = serialize_pty_test();
    let (mut session, dir) = session();
    std::fs::write(dir.path().join("multi-first.txt"), "").unwrap();
    std::fs::write(dir.path().join("multi-second.txt"), "").unwrap();

    session.exp_string("$ ").unwrap();
    session.send("cat multi-").unwrap();
    session.send("\t").unwrap();
    session.flush().unwrap();
    session.exp_string("MULTI-FIRST.TXT").unwrap();
    session.exp_string("multi-second.txt").unwrap();
    session.send_control('c').unwrap();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn history_hint_is_visible_and_right_arrow_accepts_it() {
    let _pty_test_guard = serialize_pty_test();
    let dir = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_so1sh"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "dumb")
        .env("SO1SH_SHELL_INTEGRATION", "0")
        .env("SO1SH_HIGHLIGHT", "1")
        .env("SO1SH_HISTFILE", dir.path().join("history"))
        .env("HOME", dir.path())
        .current_dir(dir.path());
    let mut session = spawn_pty(command);
    session.process_mut().set_kill_timeout(Some(1000));

    session.exp_string("$ ").unwrap();
    session.send("echo t7-history-hint").unwrap();
    session.flush().unwrap();
    session.exp_string("echo t7-history-hint").unwrap();
    session.send_line("").unwrap();
    session.exp_string("\r\n").unwrap();
    assert_no_output(&session.exp_string("t7-history-hint\r\n").unwrap());
    session.exp_string("$ ").unwrap();

    session.send("echo t7-").unwrap();
    session.flush().unwrap();
    session.exp_string("history-hint").unwrap();
    session.send("\x1b[C").unwrap();
    session.flush().unwrap();
    session.send_line("").unwrap();
    session.exp_string("\r\n").unwrap();
    assert_no_output(&session.exp_string("t7-history-hint\r\n").unwrap());
    session.exp_string("$ ").unwrap();

    session.send("echo t7-").unwrap();
    session.flush().unwrap();
    session.exp_string("history-hint").unwrap();
    session.send("\x1b[F").unwrap();
    session.flush().unwrap();
    session.send_line("").unwrap();
    session.exp_string("\r\n").unwrap();
    assert_no_output(&session.exp_string("t7-history-hint\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

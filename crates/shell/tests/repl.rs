use std::process::Command;

use rexpect::process::WaitStatus;
use rexpect::session::{PtySession, spawn_command};
use tempfile::TempDir;

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
        .env("HOME", dir.path())
        .current_dir(dir.path());
    command.args(args);
    if let Some(prompt) = prompt {
        command.env("PS1", prompt);
    }
    // rexpect disables terminal echo, so output assertions cannot match typed input.
    let mut session = spawn_command(command, Some(3000)).unwrap();
    session.process_mut().set_kill_timeout(Some(1000));
    (session, dir)
}

// Consume the editor's echoed submission before checking real command output.
fn submit(session: &mut PtySession, line: &str) {
    session.send_line(line).unwrap();
    session.exp_string("\r\n").unwrap();
}

fn assert_no_output(output: &str) {
    assert!(strip_ansi_escapes::strip(output).is_empty(), "{output:?}");
}

fn exited(session: &mut PtySession, code: i32) {
    session.exp_eof().unwrap();
    assert!(
        matches!(session.process().wait().unwrap(), WaitStatus::Exited(_, actual) if actual == code)
    );
}

#[test]
fn echo_then_prompt() {
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
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit 3");
    exited(&mut session, 3);
}

#[test]
fn control_d_exits_successfully() {
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session.send_control('d').unwrap();
    exited(&mut session, 0);
}

#[test]
fn cwd_and_functions_persist_across_lines() {
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
    let (mut session, _dir) = session_with(&[], Some("inherited> "));
    session.exp_string("inherited> ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn command_string_on_tty_does_not_enter_repl() {
    let (mut session, _dir) = session_with(&["-c", "echo hi; exit 3"], None);
    assert_no_output(&session.exp_string("hi\r\n").unwrap());
    exited(&mut session, 3);
}

#[test]
fn explicit_exit_defers_exit_trap_and_preserves_requested_status() {
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
    recover_after_error("echo ${nope?missing}; echo unreachable", "missing");
}

#[test]
fn special_builtin_redirection_error_returns_to_prompt() {
    // The session has a private cwd; this parent directory cannot exist.
    recover_after_error(": > absent/dir/x; echo unreachable", "epsh: ");
}

#[test]
fn invalid_break_operand_outside_loop_returns_to_prompt() {
    recover_after_error("break foo; echo unreachable", "break: ");
}

#[test]
fn invalid_break_operand_inside_loop_returns_to_prompt() {
    recover_after_error("while :; do break foo; echo unreachable; done", "break: ");
}

#[test]
fn invalid_continue_operand_inside_loop_returns_to_prompt() {
    recover_after_error(
        "while :; do continue foo; echo unreachable; done",
        "continue: ",
    );
}

#[test]
fn dot_missing_file_returns_to_prompt() {
    recover_after_error(". ./missing-script; echo unreachable", ".: ");
}

#[test]
fn dot_missing_argument_returns_to_prompt() {
    recover_after_error(".; echo unreachable", ".: ");
}

#[test]
fn errexit_still_ends_session_on_false() {
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
        .env("HOME", dir.path())
        .current_dir(dir.path());
    if let Some(path) = path {
        command.env("SO1SH_HISTFILE", path);
    }
    let mut session = spawn_command(command, Some(3000)).unwrap();
    session.process_mut().set_kill_timeout(Some(1000));
    session
}

#[test]
fn history_survives_sessions_and_ctrl_r_searches_it() {
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
    let dir = tempfile::tempdir().unwrap();
    history_error_keeps_session_working(history_session_with_file(&dir, Some(dir.path())));
}

#[test]
fn history_sync_error_keeps_session_working() {
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
    let tty = session.process().get_file_handle().unwrap();
    use std::os::fd::AsRawFd;
    assert_eq!(unsafe { libc::tcgetpgrp(tty.as_raw_fd()) }, jobs.0[0]);
    session
        .send(std::str::from_utf8(&[signal]).unwrap())
        .unwrap();
    session.flush().unwrap();
    if status == 148 {
        session.exp_string("stopped").unwrap();
    }
    session.exp_string("$ ").unwrap();
    submit(&mut session, "echo $?");
    assert_no_output(&session.exp_string(&format!("{status}\r\n")).unwrap());
    session.exp_string("$ ").unwrap();
    // Exited/stopped jobs must have been terminated and reaped, not orphaned.
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
    jobs.0.clear(); // Never signal an already reaped PID that could be reused.
    submit(&mut session, "echo after");
    assert_no_output(&session.exp_string("after\r\n").unwrap());
    session.exp_string("$ ").unwrap();
    submit(&mut session, "exit");
    exited(&mut session, 0);
}

#[test]
fn foreground_ctrl_c_returns_status_130() {
    foreground_signal(false, 0x03, 130);
}
#[test]
fn foreground_ctrl_backslash_returns_status_131() {
    foreground_signal(false, 0x1c, 131);
}
#[test]
fn foreground_ctrl_z_is_reported_and_cleaned_up() {
    foreground_signal(false, 0x1a, 148);
}
#[test]
fn pipeline_ctrl_c_terminates_every_stage() {
    foreground_signal(true, 0x03, 130);
}
#[test]
fn pipeline_ctrl_z_cleans_up_every_stage() {
    foreground_signal(true, 0x1a, 148);
}

#[test]
fn children_see_cooked_modes_and_job_mode_changes_are_restored() {
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

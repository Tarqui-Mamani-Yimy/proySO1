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
    session.send_line("echo hi").unwrap();
    assert_eq!(session.exp_string("hi\r\n").unwrap(), "");
    session.exp_string("$ ").unwrap();
    session.send_line("exit").unwrap();
    exited(&mut session, 0);
}

#[test]
fn variables_persist_across_lines() {
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session.send_line("x=5").unwrap();
    session.exp_string("$ ").unwrap();
    session.send_line("echo $x").unwrap();
    assert_eq!(session.exp_string("5\r\n").unwrap(), "");
    session.exp_string("$ ").unwrap();
    session.send_line("exit").unwrap();
    exited(&mut session, 0);
}

#[test]
fn exit_with_explicit_status() {
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session.send_line("exit 3").unwrap();
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
    session.send_line("exit").unwrap();
    exited(&mut session, 0);
}

#[test]
fn previous_status_and_implicit_exit() {
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session.send_line("false").unwrap();
    session.exp_string("$ ").unwrap();
    session.send_line("echo $?").unwrap();
    assert_eq!(session.exp_string("1\r\n").unwrap(), "");
    session.exp_string("$ ").unwrap();
    session.send_line("false").unwrap();
    session.exp_string("$ ").unwrap();
    session.send_line("exit").unwrap();
    exited(&mut session, 1);
}

#[test]
fn eof_preserves_status_and_defers_exit_trap() {
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session.send_line("trap 'echo goodbye' EXIT").unwrap();
    let before_prompt = session.exp_string("$ ").unwrap();
    assert_eq!(before_prompt, "");
    session.send_line("false").unwrap();
    session.exp_string("$ ").unwrap();
    session.send_control('d').unwrap();
    session.exp_string("goodbye\r\n").unwrap();
    exited(&mut session, 1);
}

#[test]
fn syntax_error_updates_status_and_keeps_prompt() {
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session.send_line("if").unwrap();
    session.exp_string("epsh: ").unwrap();
    session.exp_string("$ ").unwrap();
    session.send_line("echo $?").unwrap();
    assert_eq!(session.exp_string("2\r\n").unwrap(), "");
    session.exp_string("$ ").unwrap();
    session.send_line("exit").unwrap();
    exited(&mut session, 0);
}

#[test]
fn prompt_can_change_and_nested_exit_is_honored() {
    let (mut session, _dir) = session();
    session.exp_string("$ ").unwrap();
    session.send_line("PS1='custom> '").unwrap();
    session.exp_string("custom> ").unwrap();
    session
        .send_line("f() { exit 7; }; f; echo unreachable")
        .unwrap();
    let output = session.exp_eof().unwrap();
    assert!(!output.contains("unreachable\r\n"));
    assert!(matches!(
        session.process().wait().unwrap(),
        WaitStatus::Exited(_, 7)
    ));
}

#[test]
fn inherited_ps1_is_used() {
    let (mut session, _dir) = session_with(&[], Some("inherited> "));
    session.exp_string("inherited> ").unwrap();
    session.send_line("exit").unwrap();
    exited(&mut session, 0);
}

#[test]
fn command_string_on_tty_does_not_enter_repl() {
    let (mut session, _dir) = session_with(&["-c", "echo hi; exit 3"], None);
    assert_eq!(session.exp_string("hi\r\n").unwrap(), "");
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
    assert_eq!(output, "");
    session.send_line("exit 3").unwrap();
    session.exp_string("goodbye\r\n").unwrap();
    exited(&mut session, 3);
}

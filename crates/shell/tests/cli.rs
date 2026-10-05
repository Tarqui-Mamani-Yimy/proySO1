use std::io::Write;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

fn command(dir: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_so1sh"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .current_dir(dir.path())
        .stdin(Stdio::null());
    command
}

fn assert_output(output: Output, code: i32, stdout: &[u8]) {
    assert_eq!(output.status.code(), Some(code), "{output:?}");
    assert_eq!(output.stdout, stdout);
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn command_string_preserves_arguments_and_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let output = command(&dir)
        .args([
            "-c",
            "printf '%s\\n' \"$0\" \"$1\"; exit 3",
            "name",
            "two words",
        ])
        .output()
        .unwrap();
    assert_output(output, 3, b"name\ntwo words\n");
}

#[test]
fn script_preserves_arguments_and_non_utf8_bytes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("script.sh"),
        b"printf '%s\\n' \"$0\" \"$1\"; printf '\xff'; exit 7\n",
    )
    .unwrap();
    let output = command(&dir)
        .args(["script.sh", "two words"])
        .output()
        .unwrap();
    assert_output(output, 7, b"script.sh\ntwo words\n\xff");
}

#[test]
fn piped_input_is_a_whole_script_without_prompts() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = command(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"if true; then\necho hi\nfi\nfalse\n")
        .unwrap();
    assert_output(child.wait_with_output().unwrap(), 1, b"hi\n");
}

#[test]
fn script_options_reuse_epsh_semantics() {
    let dir = tempfile::tempdir().unwrap();
    assert_output(
        command(&dir)
            .args(["-e", "-c", "false; echo unreachable"])
            .output()
            .unwrap(),
        1,
        b"",
    );
    assert_output(
        command(&dir)
            .args(["-n", "-c", "echo unreachable"])
            .output()
            .unwrap(),
        0,
        b"",
    );
}

#[test]
fn invalid_cli_arguments_keep_epsh_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let missing_command = command(&dir).arg("-c").output().unwrap();
    assert_eq!(missing_command.status.code(), Some(2));
    assert_eq!(missing_command.stderr, b"epsh: -c requires an argument\n");
    let missing_script = command(&dir).arg("missing.sh").output().unwrap();
    assert_eq!(missing_script.status.code(), Some(127));
    assert!(missing_script.stderr.starts_with(b"epsh: missing.sh:"));
}

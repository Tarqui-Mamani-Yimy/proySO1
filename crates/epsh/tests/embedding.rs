use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epsh::ast::Program;
use epsh::builtins::{BUILTIN_NAMES, is_builtin};
use epsh::error::ExitStatus;
use epsh::eval::Shell;
use epsh::parser::Parser;
use tempfile::tempdir;

fn parse(src: &str) -> Program {
    Parser::new(src).parse().unwrap()
}

fn check_child_test(name: &str, out: &std::process::Output) -> Result<(), String> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let ran_one = stdout.lines().any(|line| line == "running 1 test");
    let passed_one = stdout
        .lines()
        .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"));
    if out.status.success() && ran_one && passed_one {
        Ok(())
    } else {
        Err(format!(
            "{name}: expected exactly one test to pass: {out:?}"
        ))
    }
}

// Tests that change FDs or reap arbitrary children need a private process.
fn isolated_test(name: &str) -> bool {
    isolated_test_capture(name).is_some()
}

fn isolated_test_capture(name: &str) -> Option<std::process::Output> {
    if std::env::var("EPSH_EMBEDDING_CHILD").as_deref() == Ok(name) {
        return None;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("EPSH_EMBEDDING_CHILD", name)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    check_child_test(name, &out).unwrap();
    Some(out)
}

mod hermeticity {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn exact_child_filter_rejects_missing_test() {
        let name = "hermeticity::this_test_does_not_exist";
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("running 0 tests"));
        let error = check_child_test(name, &out).unwrap_err();
        assert!(error.contains("exactly one test"), "{error}");
    }

    #[test]
    fn fd_tests_preserve_host_descriptors() {
        if isolated_test("hermeticity::fd_tests_preserve_host_descriptors") {
            return;
        }
        let marker = fs::File::open("/dev/null").unwrap();
        // SAFETY: this test is alone in its child process; fd 50 is private.
        assert_eq!(unsafe { libc::dup2(marker.as_raw_fd(), 50) }, 50);
        super::redirection_failures::failed_redirection_restores_earlier_descriptors();
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: stat points to writable storage; an absent fd reports EBADF.
        let result = unsafe { libc::fstat(50, stat.as_mut_ptr()) };
        // SAFETY: close reports EBADF if the baseline test closed fd 50.
        unsafe { libc::close(50) };
        assert_eq!(result, 0, "FD test closed another test's descriptor");
    }

    #[test]
    fn builtin_probe_preserves_host_children() {
        if isolated_test("hermeticity::builtin_probe_preserves_host_children") {
            return;
        }
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        super::builtin_list::builtin_names_matches_try_builtin();
        let waited = child.wait();
        // stdout remains visible even if a broken probe leaves stderr redirected.
        println!("host child wait result: {waited:?}");
        assert_eq!(
            waited
                .expect("builtin probe reaped another test's child")
                .code(),
            Some(7)
        );
    }

    #[test]
    fn output_sinks_ignore_ambient_path() {
        let dir = tempdir().unwrap();
        let impostor = dir.path().join("nonexistent_cmd_xyz");
        fs::write(&impostor, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&impostor, fs::Permissions::from_mode(0o755)).unwrap();
        for name in [
            "output_sinks::capture_stderr",
            "output_sinks::separate_stdout_stderr",
        ] {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env("PATH", dir.path())
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            check_child_test(name, &out).unwrap();
        }
    }
}

mod wait_jobs {
    use super::*;
    use std::process::{Command, Stdio};

    fn host_child() -> std::process::Child {
        Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn wait_all_preserves_host_child() {
        if isolated_test("wait_jobs::wait_all_preserves_host_child") {
            return;
        }
        let mut host = host_child();
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("(exit 11) & wait"), 0);
        assert_eq!(
            host.wait().expect("shell reaped the host child").code(),
            Some(7)
        );
    }

    #[test]
    fn wait_host_pid_is_unknown() {
        if isolated_test("wait_jobs::wait_host_pid_is_unknown") {
            return;
        }
        let mut host = host_child();
        let mut shell = Shell::new();
        let status = shell.run_script(&format!("wait {}", host.id()));
        assert_eq!(
            host.wait().expect("shell reaped the host child").code(),
            Some(7)
        );
        assert_eq!(status, 127);
    }

    #[test]
    fn shells_do_not_reap_each_others_jobs() {
        if isolated_test("wait_jobs::shells_do_not_reap_each_others_jobs") {
            return;
        }
        let mut owner = Shell::new();
        let mut other = Shell::builder().build();
        assert_eq!(owner.run_script("(exit 11) &"), 0);
        assert_eq!(other.run_script("wait"), 0);
        assert_eq!(owner.run_script("wait $!"), 11);
    }
}

mod builder {
    use super::*;

    #[test]
    fn default_builder() {
        let shell = Shell::builder().build();
        assert_eq!(shell.exit_status(), ExitStatus::SUCCESS);
    }

    #[test]
    fn builder_with_cwd() {
        let mut shell = Shell::builder().cwd(PathBuf::from("/tmp")).build();
        let status = shell.run_program(&parse("pwd"));
        assert_eq!(status, ExitStatus::SUCCESS);
    }

    #[test]
    fn builder_with_options() {
        let shell = Shell::builder()
            .errexit(true)
            .nounset(true)
            .pipefail(true)
            .build();
        assert!(shell.opts().errexit);
        assert!(shell.opts().nounset);
        assert!(shell.opts().pipefail);
    }

    #[test]
    fn builder_env_clear() {
        let mut shell = Shell::builder().env_clear().build();
        // PATH should not be inherited
        let status = shell.run_program(&parse("echo ${PATH-unset}"));
        // Can't easily check output without sink, just verify it runs
        assert_eq!(status, ExitStatus::SUCCESS);
    }

    #[test]
    fn builder_with_sinks() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .stderr_sink(stderr.clone())
            .build();
        shell.run_program(&parse("echo hello"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn builder_chained() {
        let cancel = Arc::new(AtomicBool::new(false));
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .cwd(PathBuf::from("/tmp"))
            .errexit(true)
            .cancel_flag(cancel)
            .stdout_sink(stdout.clone())
            .timeout(Duration::from_secs(60))
            .build();
        shell.run_program(&parse("echo ok"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "ok\n");
    }
}

mod parse_then_execute {
    use super::*;

    #[test]
    fn basic_parse_execute() {
        let program = parse("echo hello");
        let mut shell = Shell::new();
        let status = shell.run_program(&program);
        assert_eq!(status, ExitStatus::SUCCESS);
    }

    #[test]
    fn reuse_parsed_program() {
        let program = parse("x=$((x + 1)); echo $x");
        let mut shell = Shell::new();
        let _ = shell.set_var("x", "0");
        shell.run_program(&program);
        assert_eq!(shell.get_var("x"), Some("1"));
        shell.run_program(&program);
        assert_eq!(shell.get_var("x"), Some("2"));
    }

    #[test]
    fn parse_error_doesnt_crash() {
        let result = Parser::new("if; then; fi; (((").parse();
        assert!(result.is_err());
    }

    #[test]
    fn inspect_ast() {
        let program = parse("echo hello world");
        assert_eq!(program.commands.len(), 1);
    }
}

mod cancellation {
    use super::*;

    #[test]
    fn cancel_stops_execution() {
        let cancel = Arc::new(AtomicBool::new(false));
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .cancel_flag(cancel.clone())
            .stdout_sink(stdout.clone())
            .build();

        // Set cancel before running — should abort immediately
        cancel.store(true, Ordering::Relaxed);
        let status = shell.run_program(&parse("echo should-not-appear"));
        assert_eq!(status.code(), 130); // SIGINT
        let out = stdout.lock().unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn cancel_during_loop() {
        let cancel = Arc::new(AtomicBool::new(false));
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let cancel2 = cancel.clone();

        let mut shell = Shell::builder()
            .cancel_flag(cancel)
            .stdout_sink(stdout.clone())
            .build();

        // Cancel after a short delay
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancel2.store(true, Ordering::Relaxed);
        });

        let status = shell.run_program(&parse("while true; do echo x; done"));
        assert_eq!(status.code(), 130);
    }
}

mod timeout {
    use super::*;

    #[test]
    fn timeout_stops_execution() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .timeout(Duration::from_millis(50))
            .build();
        let status = shell.run_program(&parse("while true; do echo x; done"));
        assert_eq!(status.code(), 130);
    }

    #[test]
    fn no_timeout_if_fast() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .timeout(Duration::from_secs(10))
            .build();
        let status = shell.run_program(&parse("echo hello"));
        assert_eq!(status, ExitStatus::SUCCESS);
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn set_timeout_after_construction() {
        let mut shell = Shell::new();
        shell.set_timeout(Duration::from_millis(50));
        let status = shell.run_program(&parse("while true; do :; done"));
        assert_eq!(status.code(), 130);
    }
}

mod output_sinks {
    use super::*;

    #[test]
    fn capture_stdout() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        shell.run_program(&parse("echo hello; echo world"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "hello\nworld\n");
    }

    #[test]
    fn capture_stderr() {
        let dir = tempdir().unwrap();
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stderr_sink(stderr.clone()).build();
        shell.set_var("PATH", dir.path().to_str().unwrap()).unwrap();
        // Shell error messages go through write_err → stderr_sink
        shell.run_program(&parse("nonexistent_cmd_xyz"));
        let err = String::from_utf8_lossy(&stderr.lock().unwrap()).to_string();
        assert!(err.contains("not found"), "stderr: {err}");
    }

    #[test]
    fn capture_external_command() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        shell.run_program(&parse("/bin/echo external"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "external\n");
    }

    #[test]
    fn separate_stdout_stderr() {
        let dir = tempdir().unwrap();
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .stderr_sink(stderr.clone())
            .build();
        shell.set_var("PATH", dir.path().to_str().unwrap()).unwrap();
        // echo goes to stdout_sink, error message goes to stderr_sink
        shell.run_program(&parse("echo out; nonexistent_cmd_xyz"));
        assert_eq!(
            String::from_utf8_lossy(&stdout.lock().unwrap()).as_ref(),
            "out\n"
        );
        let err = String::from_utf8_lossy(&stderr.lock().unwrap()).to_string();
        assert!(err.contains("not found"), "stderr: {err}");
    }
}

mod cwd_isolation {
    use super::*;

    #[test]
    fn shells_have_independent_cwd() {
        let stdout1 = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stdout2 = Arc::new(Mutex::new(Vec::<u8>::new()));

        let mut shell1 = Shell::builder()
            .cwd(PathBuf::from("/tmp"))
            .stdout_sink(stdout1.clone())
            .build();
        let mut shell2 = Shell::builder()
            .cwd(PathBuf::from("/"))
            .stdout_sink(stdout2.clone())
            .build();

        shell1.run_program(&parse("pwd"));
        shell2.run_program(&parse("pwd"));

        let out1 = String::from_utf8_lossy(&stdout1.lock().unwrap()).to_string();
        let out2 = String::from_utf8_lossy(&stdout2.lock().unwrap()).to_string();
        // macOS: /tmp may resolve to /private/tmp
        assert!(out1.trim() == "/tmp" || out1.trim() == "/private/tmp");
        assert_eq!(out2.trim(), "/");
    }

    #[test]
    fn cd_updates_shell_cwd() {
        let mut shell = Shell::builder().cwd(PathBuf::from("/")).build();
        shell.run_program(&parse("cd /tmp"));
        // macOS: /tmp → /private/tmp
        let cwd = shell.cwd().to_string_lossy().to_string();
        assert!(cwd == "/tmp" || cwd == "/private/tmp");
    }

    #[test]
    fn cd_dot_navigation_preserves_canonical_paths() {
        let temp = tempdir().unwrap();
        let real = temp.path().join("real");
        let child = real.join("child");
        fs::create_dir_all(&child).unwrap();
        let canonical_child = fs::canonicalize(&child).unwrap();
        let canonical_real = fs::canonicalize(&real).unwrap();
        let canonical_temp = fs::canonicalize(temp.path()).unwrap();

        let mut shell = Shell::builder().cwd(child.clone()).build();
        assert_eq!(shell.run_program(&parse("cd .")), ExitStatus::SUCCESS);
        assert_eq!(shell.cwd(), canonical_child);

        assert_eq!(shell.run_program(&parse("cd ..")), ExitStatus::SUCCESS);
        assert_eq!(shell.cwd(), canonical_real);

        assert_eq!(shell.run_program(&parse("cd ..")), ExitStatus::SUCCESS);
        assert_eq!(shell.cwd(), canonical_temp);
    }

    #[test]
    fn cd_dot_navigation_clamps_at_root() {
        let mut shell = Shell::builder().cwd(PathBuf::from("/")).build();
        assert_eq!(
            shell.run_program(&parse("cd ../../..")),
            ExitStatus::SUCCESS
        );
        assert_eq!(shell.cwd(), PathBuf::from("/"));
    }

    #[test]
    fn cd_symlink_then_parent_uses_canonical_parent() {
        let temp = tempdir().unwrap();
        let real = temp.path().join("real");
        let child = real.join("child");
        let link = temp.path().join("link");
        fs::create_dir_all(&child).unwrap();
        symlink("real", &link).unwrap();
        let canonical_child = fs::canonicalize(&child).unwrap();
        let canonical_real = fs::canonicalize(&real).unwrap();

        let mut shell = Shell::builder().cwd(temp.path().to_path_buf()).build();
        assert_eq!(
            shell.run_program(&parse("cd link/child")),
            ExitStatus::SUCCESS
        );
        assert_eq!(shell.cwd(), canonical_child);

        assert_eq!(shell.run_program(&parse("cd ..")), ExitStatus::SUCCESS);
        assert_eq!(shell.cwd(), canonical_real);
    }

    #[test]
    fn cd_normal_component_still_resolves_symlinks() {
        let temp = tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real).unwrap();
        symlink("real", &link).unwrap();
        let canonical_real = fs::canonicalize(&real).unwrap();

        let mut shell = Shell::builder().cwd(temp.path().to_path_buf()).build();
        assert_eq!(shell.run_program(&parse("cd link")), ExitStatus::SUCCESS);
        assert_eq!(shell.cwd(), canonical_real);
    }

    #[test]
    fn failed_cd_does_not_change_cwd() {
        let temp = tempdir().unwrap();
        let start = temp.path().join("start");
        fs::create_dir(&start).unwrap();
        let mut shell = Shell::builder().cwd(start.clone()).build();

        assert_eq!(shell.run_program(&parse("cd missing")), ExitStatus::FAILURE);
        assert_eq!(shell.cwd(), start);
    }
}

mod builtin_list {
    use super::*;

    #[test]
    fn builtin_names_contains_core() {
        assert!(BUILTIN_NAMES.contains(&"echo"));
        assert!(BUILTIN_NAMES.contains(&"cd"));
        assert!(BUILTIN_NAMES.contains(&"export"));
        assert!(BUILTIN_NAMES.contains(&"test"));
        assert!(BUILTIN_NAMES.contains(&":"));
    }

    #[test]
    fn is_builtin_works() {
        assert!(is_builtin("echo"));
        assert!(is_builtin("["));
        assert!(!is_builtin("ls"));
        assert!(!is_builtin("git"));
    }

    #[test]
    pub(super) fn builtin_names_matches_try_builtin() {
        if isolated_test("builtin_list::builtin_names_matches_try_builtin") {
            return;
        }
        // Every name in BUILTIN_NAMES should be recognized by try_builtin
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout).build();
        for &name in BUILTIN_NAMES {
            // Just verify they don't crash — some need args. Redirect stdin from
            // /dev/null so interactive builtins like `read` get immediate EOF.
            let _ = shell.run_program(&parse(&format!("{name} --help </dev/null 2>/dev/null")));
        }
    }
}

mod embedder_builtins {
    use super::*;
    use epsh::shell_bytes::ShellBytes;

    fn register_hello(shell: &mut Shell) {
        shell
            .register_embedder_builtin("hello", |_shell, _argv| Ok(ExitStatus::SUCCESS))
            .unwrap();
    }

    #[test]
    fn callback_receives_argv_and_writes_to_configured_sinks() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .stderr_sink(stderr.clone())
            .build();
        shell
            .register_embedder_builtin("hello", |shell, argv| {
                assert_eq!(
                    argv,
                    &[ShellBytes::from("hello"), ShellBytes::from("world")]
                );
                shell.write_stdout(b"hello world\n");
                shell.write_stderr(b"hello diagnostic\n");
                Ok(ExitStatus::SUCCESS)
            })
            .unwrap();

        assert_eq!(
            shell.run_program(&parse("hello world")),
            ExitStatus::SUCCESS
        );
        assert_eq!(*stdout.lock().unwrap(), b"hello world\n");
        assert_eq!(*stderr.lock().unwrap(), b"hello diagnostic\n");
        assert!(shell.is_embedder_builtin_registered("hello"));
        assert!(!is_builtin("hello"));
    }

    #[test]
    fn argv_preserves_non_utf8_bytes() {
        let captured = Arc::new(Mutex::new(Vec::<ShellBytes>::new()));
        let captured_args = captured.clone();
        let mut shell = Shell::new();
        shell
            .register_embedder_builtin("hello", move |_shell, argv| {
                captured_args.lock().unwrap().extend_from_slice(argv);
                Ok(ExitStatus::SUCCESS)
            })
            .unwrap();
        let invalid = ShellBytes::from_vec(vec![0xff]);
        let encoded = epsh::encoding::bytes_to_str(invalid.as_bytes());

        assert_eq!(
            shell.run_program(&parse(&format!("hello {encoded}"))),
            ExitStatus::SUCCESS
        );
        assert_eq!(
            *captured.lock().unwrap(),
            vec![ShellBytes::from("hello"), invalid]
        );
    }

    #[test]
    fn redirections_and_pipelines_reach_embedder_builtins() {
        if isolated_test("embedder_builtins::redirections_and_pipelines_reach_embedder_builtins") {
            return;
        }
        let temp = tempdir().unwrap();
        let mut shell = Shell::builder().cwd(temp.path().to_path_buf()).build();
        shell
            .register_embedder_builtin("hello", |shell, argv| {
                shell.write_stdout(argv[1].as_bytes());
                Ok(ExitStatus::SUCCESS)
            })
            .unwrap();

        assert_eq!(
            shell.run_program(&parse("hello redirected > direct.out")),
            ExitStatus::SUCCESS
        );
        assert_eq!(
            fs::read(temp.path().join("direct.out")).unwrap(),
            b"redirected"
        );
        assert_eq!(
            shell.run_program(&parse("hello piped | cat > pipeline.out")),
            ExitStatus::SUCCESS
        );
        assert_eq!(
            fs::read(temp.path().join("pipeline.out")).unwrap(),
            b"piped"
        );
    }

    #[test]
    fn redirected_embedder_stdout_bypasses_configured_sink() {
        if isolated_test("embedder_builtins::redirected_embedder_stdout_bypasses_configured_sink") {
            return;
        }
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let temp = tempdir().unwrap();
        let mut shell = Shell::builder()
            .cwd(temp.path().to_path_buf())
            .stdout_sink(stdout.clone())
            .build();
        shell
            .register_embedder_builtin("hello", |shell, _argv| {
                shell.write_stdout(b"redirected\n");
                Ok(ExitStatus::SUCCESS)
            })
            .unwrap();

        assert_eq!(
            shell.run_program(&parse("hello > output.txt")),
            ExitStatus::SUCCESS
        );
        assert!(stdout.lock().unwrap().is_empty());
        assert_eq!(
            fs::read(temp.path().join("output.txt")).unwrap(),
            b"redirected\n"
        );
    }

    #[test]
    fn descriptor_duplications_route_output_to_the_destination_sink() {
        let name =
            "embedder_builtins::descriptor_duplications_route_output_to_the_destination_sink";
        if let Some(out) = isolated_test_capture(name) {
            for leaked in [b"a\n".as_slice(), b"b\n", b"c\n", b"d\n", b"ef\n"] {
                assert!(
                    !out.stdout
                        .windows(leaked.len())
                        .any(|window| window == leaked),
                    "stdout leaked {leaked:?}: {out:?}"
                );
                assert!(
                    !out.stderr
                        .windows(leaked.len())
                        .any(|window| window == leaked),
                    "stderr leaked {leaked:?}: {out:?}"
                );
            }
            return;
        }
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .stderr_sink(stderr.clone())
            .build();
        shell
            .register_embedder_builtin("emit", |shell, _| {
                shell.write_stdout(b"f\n");
                Ok(ExitStatus::SUCCESS)
            })
            .unwrap();
        for command in ["echo a >&2", "printf e 1>&2", "emit >&2"] {
            assert_eq!(shell.run_program(&parse(command)), ExitStatus::SUCCESS);
        }
        assert_eq!(
            shell.run_program(&parse("echo b 2>&1")),
            ExitStatus::SUCCESS
        );
        assert_eq!(
            shell.run_program(&parse("{ echo c; echo d >&2; } 2>&1")),
            ExitStatus::SUCCESS
        );
        assert_eq!(*stderr.lock().unwrap(), b"a\nef\n");
        assert_eq!(*stdout.lock().unwrap(), b"b\nc\nd\n");
    }

    #[test]
    fn embedder_panic_restores_redirected_stdout() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        if isolated_test("embedder_builtins::embedder_panic_restores_redirected_stdout") {
            return;
        }
        let temp = tempdir().unwrap();
        let output = temp.path().join("panic.txt");
        let mut shell = Shell::new();
        shell
            .register_embedder_builtin(
                "panic_builtin",
                |_shell, _argv| -> epsh::error::Result<ExitStatus> {
                    panic!("intentional embedder panic")
                },
            )
            .unwrap();

        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                shell.run_program(&parse(&format!("panic_builtin > {}", output.display())))
            }))
            .is_err()
        );
        let fd_target = fs::read_link("/proc/self/fd/1").unwrap();
        assert_ne!(fd_target, output);
    }

    #[test]
    fn times_is_an_implemented_special_builtin() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        assert_eq!(shell.run_program(&parse("times")), ExitStatus::SUCCESS);
        let output = String::from_utf8(stdout.lock().unwrap().clone()).unwrap();
        assert_eq!(output.lines().count(), 2);
        assert!(
            output
                .lines()
                .all(|line| line.contains("m") && line.contains("s"))
        );
    }

    #[test]
    fn command_substitution_runs_embedder_builtin() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        shell
            .register_embedder_builtin("hello", |shell, argv| {
                shell.write_stdout(argv[1].as_bytes());
                shell.write_stdout(b"\n");
                Ok(ExitStatus::SUCCESS)
            })
            .unwrap();

        let command = parse("hello substituted");
        assert_eq!(
            shell.command_subst(&command.commands[0]).unwrap(),
            "substituted"
        );
        assert!(stdout.lock().unwrap().is_empty());
    }

    #[test]
    fn command_v_and_type_recognize_embedder_builtins() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        register_hello(&mut shell);

        assert_eq!(
            shell.run_program(&parse("command -v hello; type hello")),
            ExitStatus::SUCCESS
        );
        assert_eq!(
            *stdout.lock().unwrap(),
            b"hello\nhello is a shell builtin\n"
        );
    }

    #[test]
    fn function_takes_precedence_over_embedder_builtin() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        shell
            .register_embedder_builtin("hello", |shell, _argv| {
                shell.write_stdout(b"embedder\n");
                Ok(ExitStatus::SUCCESS)
            })
            .unwrap();

        assert_eq!(
            shell.run_program(&parse("hello() { echo function; }; hello")),
            ExitStatus::SUCCESS
        );
        assert_eq!(*stdout.lock().unwrap(), b"function\n");
    }

    #[test]
    fn function_takes_precedence_over_regular_epsh_builtin() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();

        assert_eq!(
            shell.run_program(&parse("echo() { command echo function; }; echo")),
            ExitStatus::SUCCESS
        );
        assert_eq!(*stdout.lock().unwrap(), b"function\n");
    }

    #[test]
    fn cd_wrapper_function_precedes_regular_builtin() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        assert_eq!(
            shell.run_program(&parse(&format!(
                "cd() {{ echo wrapper; command cd '{}'; }}; cd",
                target.display()
            ))),
            ExitStatus::SUCCESS
        );
        assert_eq!(*stdout.lock().unwrap(), b"wrapper\n");
        assert_eq!(shell.cwd(), target);
    }

    #[test]
    fn regular_builtin_shadowing_requires_explicit_opt_in() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        assert!(
            shell
                .register_embedder_builtin("echo", |_shell, _argv| Ok(ExitStatus::SUCCESS))
                .is_err()
        );
        assert!(
            shell
                .register_embedder_builtin_shadowing_regular("echo", |shell, _argv| {
                    shell.write_stdout(b"embedder echo\n");
                    Ok(ExitStatus::SUCCESS)
                })
                .is_ok()
        );
        assert!(
            shell
                .register_embedder_builtin_shadowing_regular("exit", |_shell, _argv| {
                    Ok(ExitStatus::SUCCESS)
                })
                .is_err()
        );
        assert!(
            shell
                .register_embedder_builtin("times", |_shell, _argv| { Ok(ExitStatus::SUCCESS) })
                .is_err()
        );
        assert_eq!(
            shell.run_program(&parse("echo ordinary?")),
            ExitStatus::SUCCESS
        );
        assert_eq!(*stdout.lock().unwrap(), b"embedder echo\n");
    }

    #[test]
    fn command_prefix_invokes_registered_builtin() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        shell
            .register_embedder_builtin("hello", |shell, argv| {
                shell.write_stdout(argv[1].as_bytes());
                shell.write_stdout(b"\n");
                Ok(ExitStatus::SUCCESS)
            })
            .unwrap();
        assert_eq!(
            shell.run_program(&parse("command hello dispatched")),
            ExitStatus::SUCCESS
        );
        assert_eq!(*stdout.lock().unwrap(), b"dispatched\n");
    }

    #[test]
    fn callback_shell_errors_propagate_through_normal_error_handling() {
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stderr_sink(stderr.clone()).build();
        shell
            .register_embedder_builtin("hello", |_shell, _argv| {
                Err(epsh::error::ShellError::Runtime {
                    msg: "callback failed".into(),
                    span: Default::default(),
                })
            })
            .unwrap();
        assert_eq!(shell.run_program(&parse("hello")), ExitStatus::MISUSE);
        assert_eq!(*stderr.lock().unwrap(), b"epsh: callback failed\n");
    }
}

mod external_handler {
    use super::*;
    use epsh::eval::ExternalHandler;
    use epsh::shell_bytes::ShellBytes;

    #[test]
    fn handler_receives_args() {
        let captured_args = Arc::new(Mutex::new(Vec::<Vec<ShellBytes>>::new()));
        let args_ref = captured_args.clone();
        let handler: ExternalHandler = Box::new(move |args, _env| {
            args_ref.lock().unwrap().push(args.to_vec());
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        shell.run_program(&parse("nonexistent_cmd foo bar"));
        let calls = captured_args.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            vec![
                ShellBytes::from("nonexistent_cmd"),
                ShellBytes::from("foo"),
                ShellBytes::from("bar"),
            ]
        );
    }

    #[test]
    fn handler_receives_env_pairs() {
        let captured_env = Arc::new(Mutex::new(Vec::<Vec<(ShellBytes, ShellBytes)>>::new()));
        let env_ref = captured_env.clone();
        let handler: ExternalHandler = Box::new(move |_args, env| {
            env_ref.lock().unwrap().push(env.to_vec());
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        shell.run_program(&parse("FOO=bar BAZ=qux mycmd"));
        let calls = captured_env.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains(&(ShellBytes::from("FOO"), ShellBytes::from("bar"))));
        assert!(calls[0].contains(&(ShellBytes::from("BAZ"), ShellBytes::from("qux"))));
    }

    #[test]
    fn handler_receives_exported_env() {
        let captured_env = Arc::new(Mutex::new(Vec::<Vec<(ShellBytes, ShellBytes)>>::new()));
        let env_ref = captured_env.clone();
        let handler: ExternalHandler = Box::new(move |_args, env| {
            env_ref.lock().unwrap().push(env.to_vec());
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        shell.run_program(&parse("X=set_val; export X; mycmd"));
        let calls = captured_env.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains(&(ShellBytes::from("X"), ShellBytes::from("set_val"))));
    }

    #[test]
    fn handler_receives_complete_store_environment_without_unset_values() {
        let captured_env = Arc::new(Mutex::new(Vec::<Vec<(ShellBytes, ShellBytes)>>::new()));
        let env_ref = captured_env.clone();
        let handler: ExternalHandler = Box::new(move |_args, env| {
            env_ref.lock().unwrap().push(env.to_vec());
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder()
            .env_clear()
            .external_handler(handler)
            .build();
        shell.run_program(&parse(
            "export KEEP=stored REMOVE=gone; unset REMOVE; PREFIX=prefix mycmd",
        ));

        let calls = captured_env.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let mut entries = calls[0].clone();
        entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        assert_eq!(
            entries,
            vec![
                (ShellBytes::from("KEEP"), ShellBytes::from("stored")),
                (ShellBytes::from("PREFIX"), ShellBytes::from("prefix")),
            ]
        );
    }

    #[test]
    fn handler_not_called_for_builtins() {
        let call_count = Arc::new(Mutex::new(0));
        let count_ref = call_count.clone();
        let handler: ExternalHandler = Box::new(move |_args, _env| {
            *count_ref.lock().unwrap() += 1;
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        shell.run_program(&parse("echo hello; true; false; : ; pwd"));
        assert_eq!(*call_count.lock().unwrap(), 0);
    }

    #[test]
    fn command_builtin_uses_handler() {
        let captured_args = Arc::new(Mutex::new(Vec::<Vec<ShellBytes>>::new()));
        let args_ref = captured_args.clone();
        let handler: ExternalHandler = Box::new(move |args, _env| {
            args_ref.lock().unwrap().push(args.to_vec());
            Ok(ExitStatus::from(7))
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        let status = shell.run_program(&parse("command mycmd arg"));
        assert_eq!(status.code(), 7);
        assert_eq!(
            *captured_args.lock().unwrap(),
            vec![vec![ShellBytes::from("mycmd"), ShellBytes::from("arg")]]
        );
    }

    #[test]
    fn handler_sees_shell_working_directory() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_ref = seen.clone();
        let handler: ExternalHandler = Box::new(move |_args, _env| {
            seen_ref
                .lock()
                .unwrap()
                .push(epsh::eval::external_command_cwd());
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder()
            .cwd(dir.path().to_path_buf())
            .external_handler(handler)
            .build();
        shell.run_program(&parse("mycmd; cd sub && mycmd"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].as_deref(), Some(dir.path()));
        assert_eq!(
            seen[1].as_ref().map(|p| p.canonicalize().unwrap()),
            Some(sub.canonicalize().unwrap())
        );
        assert_eq!(epsh::eval::external_command_cwd(), None);
    }

    #[test]
    fn handler_exit_status_propagates() {
        let handler: ExternalHandler = Box::new(|_args, _env| Ok(ExitStatus::from(42)));
        let mut shell = Shell::builder().external_handler(handler).build();
        let status = shell.run_program(&parse("mycmd"));
        assert_eq!(status.code(), 42);
    }
}

mod redirection_failures {
    use super::*;
    use std::os::fd::AsRawFd;

    fn descriptor_is_open(fd: i32) -> bool {
        // SAFETY: the descriptor is only borrowed for a flags query.
        rustix::io::fcntl_getfd(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }).is_ok()
    }

    #[test]
    pub(super) fn failed_redirection_restores_earlier_descriptors() {
        if isolated_test("redirection_failures::failed_redirection_restores_earlier_descriptors") {
            return;
        }
        let dir = tempdir().unwrap();
        let marker = fs::File::open(dir.path()).unwrap();
        let marker_stat = rustix::fs::fstat(&marker).unwrap();
        // SAFETY: fd 50 is reserved for this test and closed at its end.
        assert_eq!(unsafe { libc::dup2(marker.as_raw_fd(), 50) }, 50);
        let mut shell = Shell::builder()
            .cwd(dir.path().to_path_buf())
            .stderr_sink(Arc::new(Mutex::new(Vec::<u8>::new())))
            .build();
        let status = shell.run_program(&parse("printf never 50>first 51>second 52>missing/dir/x"));
        assert_ne!(status.code(), 0);
        // fd 50 points at the original object again; 51 and 52 were closed
        // before and are closed again.
        let restored =
            rustix::fs::fstat(unsafe { std::os::fd::BorrowedFd::borrow_raw(50) }).unwrap();
        assert_eq!(
            (restored.st_dev, restored.st_ino),
            (marker_stat.st_dev, marker_stat.st_ino)
        );
        assert!(!descriptor_is_open(51));
        assert!(!descriptor_is_open(52));
        // SAFETY: fd 50 was duplicated above and is not used elsewhere.
        unsafe { rustix::io::close(50) };
    }
}

mod redirect_open_handler {
    use super::*;
    use epsh::eval::{RedirectOpenHandler, RedirectOpenMode};
    use std::os::fd::{AsFd, OwnedFd};

    fn descriptor_is_open(fd: i32) -> bool {
        // SAFETY: the descriptor is only borrowed for a flags query.
        rustix::io::fcntl_getfd(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }).is_ok()
    }

    type Requests = Arc<Mutex<Vec<(PathBuf, RedirectOpenMode, i32)>>>;

    fn recording_provider(
        requests: Requests,
        opened: PathBuf,
        fail_name: Option<&'static str>,
    ) -> RedirectOpenHandler {
        Box::new(move |request| {
            requests
                .lock()
                .unwrap()
                .push((request.path.to_path_buf(), request.mode, request.fd));
            if fail_name.is_some_and(|name| request.path.ends_with(name)) {
                return Some(Err(std::io::Error::other("provider refused")));
            }
            if request.path.ends_with("default") {
                return None;
            }
            let file = fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&opened)
                .map(OwnedFd::from);
            Some(file)
        })
    }

    #[test]
    fn provider_owns_writable_opens_in_order() {
        if isolated_test("redirect_open_handler::provider_owns_writable_opens_in_order") {
            return;
        }
        let dir = tempdir().unwrap();
        let opened = dir.path().join("provided");
        let requests = Requests::default();
        let mut shell = Shell::builder()
            .cwd(dir.path().to_path_buf())
            .stdout_sink(Arc::new(Mutex::new(Vec::<u8>::new())))
            .redirect_open_handler(recording_provider(requests.clone(), opened.clone(), None))
            .build();
        let status = shell.run_program(&parse(
            "printf one 57>a 58>>b 59<>c; printf two 57>|d; printf three 57>default",
        ));
        assert_eq!(status.code(), 0);
        let requests = requests.lock().unwrap();
        let expect = |name: &str, mode, fd| (dir.path().join(name), mode, fd);
        assert_eq!(
            *requests,
            vec![
                expect("a", RedirectOpenMode::Truncate, 57),
                expect("b", RedirectOpenMode::Append, 58),
                expect("c", RedirectOpenMode::ReadWrite, 59),
                expect("d", RedirectOpenMode::Truncate, 57),
                expect("default", RedirectOpenMode::Truncate, 57),
            ]
        );
        // The provider's descriptors were used instead of path opens.
        for name in ["a", "b", "c", "d"] {
            assert!(!dir.path().join(name).exists(), "{name} was opened by path");
        }
        assert!(opened.exists());
        // Declining falls back to epsh's own open.
        assert!(dir.path().join("default").exists());
        for fd in [57, 58, 59] {
            assert!(!descriptor_is_open(fd), "fd {fd} leaked after the command");
        }
    }

    #[test]
    fn provider_error_restores_earlier_redirections() {
        if isolated_test("redirect_open_handler::provider_error_restores_earlier_redirections") {
            return;
        }
        let dir = tempdir().unwrap();
        let opened = dir.path().join("provided");
        let requests = Requests::default();
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let marker = fs::File::open(dir.path()).unwrap();
        let marker_stat = rustix::fs::fstat(marker.as_fd()).unwrap();
        // SAFETY: fd 60 is reserved for this test and closed at its end.
        assert_eq!(
            unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&marker), 60) },
            60
        );
        let mut shell = Shell::builder()
            .cwd(dir.path().to_path_buf())
            .stderr_sink(stderr.clone())
            .redirect_open_handler(recording_provider(
                requests.clone(),
                opened,
                Some("refused"),
            ))
            .build();
        let status = shell.run_program(&parse("printf never 60>first 61>first2 62>refused"));
        assert_ne!(status.code(), 0);
        assert_eq!(requests.lock().unwrap().len(), 3);
        let message = String::from_utf8(stderr.lock().unwrap().clone()).unwrap();
        assert!(message.contains("refused: provider refused"), "{message}");
        // fd 60 was open before and points at the same object again; fds 61
        // and 62 were closed before and are closed again.
        let restored =
            rustix::fs::fstat(unsafe { std::os::fd::BorrowedFd::borrow_raw(60) }).unwrap();
        assert_eq!(
            (restored.st_dev, restored.st_ino),
            (marker_stat.st_dev, marker_stat.st_ino)
        );
        assert!(!descriptor_is_open(61));
        assert!(!descriptor_is_open(62));
        // SAFETY: fd 60 was duplicated above and is not used elsewhere.
        unsafe { rustix::io::close(60) };
    }
}

mod variables {
    use super::*;

    #[test]
    fn set_get_var() {
        let mut shell = Shell::new();
        let _ = shell.set_var("MY_VAR", "hello");
        assert_eq!(shell.get_var("MY_VAR"), Some("hello"));
    }

    #[test]
    fn var_persists_across_commands() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        shell.run_program(&parse("X=42"));
        shell.run_program(&parse("echo $X"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "42\n");
    }

    #[test]
    fn exit_status_accessible() {
        let mut shell = Shell::new();
        shell.run_program(&parse("false"));
        assert_eq!(shell.exit_status(), ExitStatus::FAILURE);
        shell.run_program(&parse("true"));
        assert_eq!(shell.exit_status(), ExitStatus::SUCCESS);
    }
}

mod repl_errors {
    use super::*;
    use epsh::error::ShellError;
    use std::ops::ControlFlow;

    type Sink = Arc<Mutex<Vec<u8>>>;

    fn captured_shell() -> (Shell, Sink, Sink) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let shell = Shell::builder()
            .env_clear()
            .stdout_sink(output.clone())
            .stderr_sink(errors.clone())
            .build();
        (shell, output, errors)
    }

    fn continued(shell: &mut Shell, source: &str, status: i32) {
        assert_eq!(
            shell.eval_repl_line(source),
            ControlFlow::Continue(ExitStatus::from(status)),
            "{source}"
        );
        assert_eq!(shell.exit_status().code(), status);
    }

    #[test]
    fn shell_error_preserves_exit_trap_until_session_finishes() {
        let (mut shell, output, errors) = captured_shell();
        continued(&mut shell, "trap 'echo goodbye' EXIT", 0);
        continued(&mut shell, "echo ${nope?missing}; echo unreachable", 2);
        assert!(output.lock().unwrap().is_empty());
        assert!(String::from_utf8_lossy(&errors.lock().unwrap()).contains("missing"));
        continued(&mut shell, "echo $?; echo after", 0);
        assert_eq!(*output.lock().unwrap(), b"2\nafter\n");
        assert_eq!(shell.finish_repl().code(), 0);
        assert_eq!(*output.lock().unwrap(), b"2\nafter\ngoodbye\n");
    }

    #[test]
    fn builtin_errors_recover_but_explicit_exit_does_not() {
        let dir = tempdir().unwrap();
        for source in [
            "break foo",
            "break 0",
            "continue foo",
            "continue 0",
            ".",
            ". ./missing-script",
        ] {
            let (mut shell, output, errors) = captured_shell();
            shell.set_cwd(dir.path().to_path_buf());
            continued(&mut shell, &format!("{source}; echo unreachable"), 2);
            assert!(output.lock().unwrap().is_empty());
            assert!(!errors.lock().unwrap().is_empty());
            assert_eq!(
                shell.eval_repl_line("exit 2"),
                ControlFlow::Break(ExitStatus::MISUSE)
            );
        }
    }

    #[test]
    fn cancelled_line_continues_with_status_130() {
        let (mut shell, output, errors) = captured_shell();
        let cancel = Arc::new(AtomicBool::new(true));
        shell.set_cancel_flag(cancel.clone());
        continued(&mut shell, "echo unreachable", 130);
        assert!(output.lock().unwrap().is_empty());
        assert!(String::from_utf8_lossy(&errors.lock().unwrap()).contains("cancelled"));
        cancel.store(false, Ordering::Relaxed);
        continued(&mut shell, "echo $?; echo after", 0);
        assert_eq!(*output.lock().unwrap(), b"130\nafter\n");
    }

    #[test]
    fn recovery_unwinds_condition_and_loop_context() {
        for source in [
            "echo ${nope?missing} && echo unreachable",
            "if echo ${nope?missing}; then echo unreachable; fi",
            "while echo ${nope?missing}; do echo unreachable; done",
            "until echo ${nope?missing}; do echo unreachable; done",
        ] {
            let (mut shell, output, _) = captured_shell();
            continued(&mut shell, source, 2);
            assert!(output.lock().unwrap().is_empty());
            continued(&mut shell, "break", 0);
            continued(&mut shell, "set -e", 0);
            assert_eq!(
                shell.eval_repl_line("false"),
                ControlFlow::Break(ExitStatus::FAILURE)
            );
        }
    }

    #[test]
    fn errexit_still_terminates_on_shell_errors() {
        for source in ["echo ${nope?missing}", "break foo"] {
            let (mut shell, output, _) = captured_shell();
            continued(&mut shell, "set -e", 0);
            assert_eq!(
                shell.eval_repl_line(source),
                ControlFlow::Break(ExitStatus::MISUSE)
            );
            assert!(output.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn timed_out_line_continues_with_status_130() {
        let (mut shell, output, errors) = captured_shell();
        shell.set_timeout(Duration::ZERO);
        continued(&mut shell, "echo unreachable", 130);
        assert!(output.lock().unwrap().is_empty());
        assert!(String::from_utf8_lossy(&errors.lock().unwrap()).contains("timed out"));
        shell.set_timeout(Duration::from_secs(60));
        continued(&mut shell, "echo $?; echo after", 0);
        assert_eq!(*output.lock().unwrap(), b"130\nafter\n");
    }

    #[test]
    fn stopped_line_continues_with_status_148() {
        let (mut shell, output, errors) = captured_shell();
        // Synthetic handler result: no child process or job-control state is created.
        shell.set_external_handler(Box::new(|_, _| {
            Err(ShellError::Stopped {
                pid: 123,
                pgid: 123,
            })
        }));
        continued(&mut shell, "host-command; echo unreachable", 148);
        assert!(output.lock().unwrap().is_empty());
        assert!(String::from_utf8_lossy(&errors.lock().unwrap()).contains("stopped"));
        continued(&mut shell, "echo $?; echo after", 0);
        assert_eq!(*output.lock().unwrap(), b"148\nafter\n");
    }

    #[test]
    fn script_builtin_errors_keep_original_status_and_exit_trap() {
        let dir = tempdir().unwrap();
        for (source, status) in [
            ("break foo", 1),
            ("break 0", 1),
            ("continue foo", 1),
            ("continue 0", 1),
            (".", 2),
            (". ./missing-script", 127),
        ] {
            let (mut shell, output, errors) = captured_shell();
            shell.set_cwd(dir.path().to_path_buf());
            assert_eq!(
                shell.run_script(&format!(
                    "trap 'echo goodbye' EXIT; {source}; echo unreachable"
                )),
                status
            );
            assert_eq!(*output.lock().unwrap(), b"goodbye\n");
            assert!(!errors.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn script_control_errors_keep_original_status_and_output() {
        for (error, status) in [
            (ShellError::Cancelled, 130),
            (ShellError::TimedOut, 130),
            (
                ShellError::Stopped {
                    pid: 123,
                    pgid: 123,
                },
                148,
            ),
        ] {
            let (mut shell, output, errors) = captured_shell();
            let mut error = Some(error);
            shell.set_external_handler(Box::new(move |_, _| Err(error.take().unwrap())));
            assert_eq!(shell.run_script("host-command; echo unreachable"), status);
            assert!(output.lock().unwrap().is_empty());
            assert!(errors.lock().unwrap().is_empty());
        }
    }
}

mod interactive_signals {
    use super::*;

    #[test]
    fn children_reset_ignored_signals_and_report_signal_status() {
        if isolated_test(
            "interactive_signals::children_reset_ignored_signals_and_report_signal_status",
        ) {
            return;
        }
        // Private test process: never change the runner's signal dispositions.
        for signal in [
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTSTP,
            libc::SIGTTIN,
            libc::SIGTTOU,
        ] {
            unsafe {
                libc::signal(signal, libc::SIG_IGN);
            }
        }
        let mut shell = Shell::builder()
            .env_clear()
            .interactive(true)
            .pipefail(true)
            .build();
        shell.set_var("PATH", "/usr/bin:/bin").unwrap();
        for (signal, status) in [("INT", 130), ("QUIT", 131)] {
            assert_eq!(
                shell.run_script(&format!("/bin/sh -c 'kill -{signal} $$'")),
                status
            );
            assert_eq!(
                shell.run_script(&format!("/bin/sh -c 'kill -{signal} $$' | cat")),
                status
            );
        }
    }
}

mod stopped_job_cleanup {
    use super::*;
    #[test]
    fn cleanup_reaps_only_owned_stopped_children() {
        if isolated_test("stopped_job_cleanup::cleanup_reaps_only_owned_stopped_children") {
            return;
        }
        let dir = tempdir().unwrap();
        let mut host = std::process::Command::new("/bin/sleep")
            .arg("0.05")
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut shell = Shell::builder()
            .env_clear()
            .interactive(true)
            .cwd(dir.path().to_path_buf())
            .build();
        // SIGSTOP remains effective in an orphaned group even without a tty.
        assert_eq!(
            shell.run_script("/bin/sh -c 'echo $$ > job; kill -STOP $$'"),
            148
        );
        let pid: i32 = fs::read_to_string(dir.path().join("job"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0); // embedding Stopped stays live
        shell.discard_stopped_jobs();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert!(host.wait().unwrap().success());
        // Repeated cleanup is harmless; an external-handler host job is not adopted.
        shell.set_external_handler(Box::new(move |_, _| {
            Err(epsh::error::ShellError::Stopped {
                pid: std::process::id() as i32,
                pgid: std::process::id() as i32,
            })
        }));
        assert_eq!(
            shell.eval_repl_line("host-command"),
            std::ops::ControlFlow::Continue(ExitStatus::from(148))
        );
        shell.discard_stopped_jobs();
    }
}

mod interactive_line_interrupts {
    use super::*;

    #[test]
    fn signal_to_evaluator_reaps_every_pipeline_stage() {
        if isolated_test(
            "interactive_line_interrupts::signal_to_evaluator_reaps_every_pipeline_stage",
        ) {
            return;
        }
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let interrupt = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !["first", "second", "third"].iter().all(|name| {
                fs::read_to_string(path.join(name))
                    .is_ok_and(|raw| raw.trim().parse::<i32>().is_ok())
            }) {
                assert!(std::time::Instant::now() < deadline, "pipeline not ready");
                std::thread::sleep(Duration::from_millis(1));
            }
            // Only this isolated engine process, not the test runner, receives INT.
            unsafe {
                libc::kill(std::process::id() as i32, libc::SIGINT);
            }
        });
        let mut shell = Shell::builder()
            .env_clear()
            .interactive(true)
            .cwd(dir.path().to_path_buf())
            .build();
        let result = shell.eval_repl_line("/bin/sh -c 'echo $$ > first; exec /bin/sleep 100' | /bin/sh -c 'echo $$ > second; exec /bin/sleep 100' | /bin/sh -c 'echo $$ > third; exec /bin/sleep 100'; echo forbidden");
        interrupt.join().unwrap();
        assert_eq!(
            result,
            std::ops::ControlFlow::Continue(ExitStatus::from(130))
        );
        for name in ["first", "second", "third"] {
            let pid: i32 = fs::read_to_string(dir.path().join(name))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "unreaped {name}: {pid}");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }

    #[test]
    fn actual_signals_abort_the_line_but_numeric_statuses_do_not() {
        if isolated_test(
            "interactive_line_interrupts::actual_signals_abort_the_line_but_numeric_statuses_do_not",
        ) {
            return;
        }
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .env_clear()
            .interactive(true)
            .stdout_sink(out.clone())
            .build();
        shell.set_var("PATH", "/usr/bin:/bin").unwrap();
        for (signal, code) in [("INT", 130), ("QUIT", 131)] {
            for pipeline in [false, true] {
                out.lock().unwrap().clear();
                let suffix = if pipeline { " | /bin/cat" } else { "" };
                assert_eq!(
                    shell.run_script(&format!(
                        "/bin/sh -c 'kill -{signal} $$'{suffix}; echo forbidden"
                    )),
                    code
                );
                assert!(out.lock().unwrap().is_empty());
            }
            out.lock().unwrap().clear();
            assert_eq!(
                shell.run_script(&format!("/bin/sh -c 'exit {code}'; echo allowed")),
                0
            );
            assert_eq!(&*out.lock().unwrap(), b"allowed\n");
        }
        // Non-interactive lists retain their existing behavior.
        shell.opts_mut().interactive = false;
        out.lock().unwrap().clear();
        assert_eq!(shell.run_script("/bin/sh -c 'kill -INT $$'; echo batch"), 0);
        assert_eq!(&*out.lock().unwrap(), b"batch\n");
    }
}

mod interactive_wait {
    use super::*;

    #[test]
    fn cancel_and_timeout_never_signal_the_shell_group() {
        if isolated_test("interactive_wait::cancel_and_timeout_never_signal_the_shell_group") {
            return;
        }
        // Confine the baseline SIGKILL to this private test and its children.
        assert_eq!(unsafe { libc::setpgid(0, 0) }, 0);
        let group = unsafe { libc::getpgrp() };
        for timeout in [false, true] {
            let temp = tempdir().unwrap();
            let release_handler = temp.path().join("release-handler");
            let handler_release_path = release_handler.clone();
            let mut host = std::process::Command::new("/bin/sh")
                .env_clear()
                .args(["-c", "/bin/sleep 0.1; exit 7"])
                .spawn()
                .unwrap();
            let mut shell = Shell::builder()
                .env_clear()
                .interactive(true)
                .external_handler(Box::new(move |_, _| {
                    // Wait until the parent has completed its own setpgid call
                    // for the background job before joining the shell's group.
                    while !handler_release_path.exists() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    // Runs in the engine-owned background fork, not the host.
                    assert_eq!(unsafe { libc::setpgid(0, group) }, 0);
                    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) }, 0);
                    loop {
                        unsafe { libc::pause() };
                    }
                }))
                .build();
            assert_eq!(shell.run_script("group-child & owned=$!"), 0);
            fs::write(&release_handler, b"go").unwrap();
            let owned: i32 = shell.get_var("owned").unwrap().parse().unwrap();
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(owned, &mut status, libc::WUNTRACED) },
                owned
            );
            assert!(libc::WIFSTOPPED(status));
            assert_eq!(unsafe { libc::getpgid(owned) }, group);
            if timeout {
                shell.set_timeout(Duration::ZERO);
            } else {
                shell.set_cancel_flag(Arc::new(AtomicBool::new(true)));
            }
            assert_eq!(
                shell.eval_repl_line(":"),
                std::ops::ControlFlow::Continue(ExitStatus::from(130))
            );
            assert_eq!(unsafe { libc::kill(owned, 0) }, -1, "owned child survived");
            assert_eq!(
                host.wait().unwrap().code(),
                Some(7),
                "host child was killed"
            );
        }
    }

    fn run_cancelled(pipeline: bool, timeout: bool) {
        let dir = tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let file = dir.path().join("child");
        let watchdog = {
            let cancel = cancel.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                loop {
                    if done.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Ok(raw) = fs::read_to_string(&file)
                        && let Ok(pid) = raw.trim().parse::<i32>()
                    {
                        if !timeout {
                            cancel.store(true, Ordering::Relaxed);
                        }
                        if std::time::Instant::now() >= deadline {
                            // Backstop only: a failing wait must not leave a 100s child.
                            unsafe {
                                libc::kill(pid, libc::SIGKILL);
                            }
                            break;
                        }
                    }
                    assert!(
                        std::time::Instant::now() < deadline + Duration::from_secs(1),
                        "child never started"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut builder = Shell::builder()
            .env_clear()
            .interactive(true)
            .cwd(dir.path().to_path_buf())
            .cancel_flag(cancel);
        if timeout {
            builder = builder.timeout(Duration::from_millis(100));
        }
        if !pipeline {
            builder = builder
                .stdout_sink(stdout.clone())
                .stderr_sink(stderr.clone());
        }
        let mut shell = builder.build();
        let script = if pipeline {
            "/bin/true | /bin/sh -c 'echo $$ > child; exec /bin/sleep 100'"
        } else {
            "/bin/sh -c 'echo out; echo err >&2; echo $$ > child; exec /bin/sleep 100'"
        };
        let start = std::time::Instant::now();
        let status = shell.run_script(script);
        let elapsed = start.elapsed();
        done.store(true, Ordering::Relaxed);
        watchdog.join().unwrap();
        assert_eq!(status, 130);
        assert!(
            elapsed < Duration::from_secs(1),
            "interactive wait took {elapsed:?}"
        );
        if !pipeline {
            assert_eq!(&*stdout.lock().unwrap(), b"out\n");
            assert_eq!(&*stderr.lock().unwrap(), b"err\n");
        }
        let pid: i32 = fs::read_to_string(dir.path().join("child"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "child still alive");
    }

    #[test]
    fn standalone_cancel_and_timeout_join_relays() {
        if isolated_test("interactive_wait::standalone_cancel_and_timeout_join_relays") {
            return;
        }
        run_cancelled(false, false);
        run_cancelled(false, true);
    }
    #[test]
    fn pipeline_cancel_targets_the_job_group() {
        if isolated_test("interactive_wait::pipeline_cancel_targets_the_job_group") {
            return;
        }
        run_cancelled(true, false);
    }
    #[test]
    fn pipeline_timeout_targets_the_job_group() {
        if isolated_test("interactive_wait::pipeline_timeout_targets_the_job_group") {
            return;
        }
        run_cancelled(true, true);
    }

    #[test]
    fn stopped_cleanup_never_resumes_user_code() {
        if isolated_test("interactive_wait::stopped_cleanup_never_resumes_user_code") {
            return;
        }
        let dir = tempdir().unwrap();
        let mut shell = Shell::builder()
            .env_clear()
            .interactive(true)
            .cwd(dir.path().to_path_buf())
            .build();
        assert_eq!(shell.run_script("/bin/sh -c 'trap \"echo resumed > resumed\" CONT; trap \"\" TERM; kill -STOP $$; while :; do /bin/sleep 0.1; done'"), 148);
        shell.discard_stopped_jobs();
        assert!(
            !dir.path().join("resumed").exists(),
            "cleanup resumed user code"
        );
    }

    #[test]
    fn stopped_cleanup_joins_retained_relay_threads() {
        if isolated_test("interactive_wait::stopped_cleanup_joins_retained_relay_threads") {
            return;
        }
        cleanup_joins_retained_relay_threads(false);
    }

    #[test]
    fn cancel_cleanup_joins_stopped_relays() {
        if isolated_test("interactive_wait::cancel_cleanup_joins_stopped_relays") {
            return;
        }
        cleanup_joins_retained_relay_threads(true);
    }

    fn cleanup_joins_retained_relay_threads(cancelled: bool) {
        struct DelayedSink {
            started: Arc<AtomicBool>,
            release: Arc<AtomicBool>,
            finished: Arc<AtomicBool>,
        }
        impl std::io::Write for DelayedSink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.started.store(true, Ordering::Relaxed);
                while !self.release.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                self.finished.store(true, Ordering::Relaxed);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let sink = Arc::new(Mutex::new(DelayedSink {
            started: started.clone(),
            release: release.clone(),
            finished: finished.clone(),
        }));
        let mut shell = Shell::builder()
            .env_clear()
            .interactive(true)
            .stdout_sink(sink)
            .build();
        assert_eq!(
            shell.run_script("/bin/sh -c 'echo delayed; kill -STOP $$'"),
            148
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !started.load(Ordering::Relaxed) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let unblock = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            release.store(true, Ordering::Relaxed);
        });
        if cancelled {
            shell.set_cancel_flag(Arc::new(AtomicBool::new(true)));
            assert_eq!(
                shell.eval_repl_line(":"),
                std::ops::ControlFlow::Continue(ExitStatus::from(130))
            );
        } else {
            shell.discard_stopped_jobs();
        }
        let joined = finished.load(Ordering::Relaxed);
        unblock.join().unwrap();
        assert!(joined, "cleanup returned before its relay thread finished");
    }
}

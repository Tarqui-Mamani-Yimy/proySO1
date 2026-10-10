#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        path::Path,
        thread,
        time::{Duration, Instant},
    };

    fn wait_for(pty: &mut Pty, needle: &[u8]) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            while let Some(chunk) = pty.try_read() {
                output.extend(chunk);
            }
            if output.windows(needle.len()).any(|w| w == needle) {
                return output;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "did not observe {:?}; output: {:?}",
            needle,
            String::from_utf8_lossy(&output)
        );
    }

    #[test]
    fn streams_input_resizes_and_observes_exit() {
        let mut pty = Pty::spawn("/bin/sh", Path::new("/"), 80, 24).unwrap();
        pty.write(b"printf 'PTY_MARKER\\n'; read value; stty size; printf 'GOT:%s\\n' \"$value\"; exit 23\n").unwrap();
        let initial = wait_for(&mut pty, b"PTY_MARKER");
        assert!(String::from_utf8_lossy(&initial).contains("PTY_MARKER"));
        pty.resize(91, 37, 0, 0).unwrap();
        pty.write(b"input-value\n").unwrap();
        let output = wait_for(&mut pty, b"GOT:input-value");
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&initial),
            String::from_utf8_lossy(&output)
        );
        assert!(output.contains("37 91"));
        assert!(output.contains("GOT:input-value"));
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = pty.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "child did not exit");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.exit_code(), 23);
    }

    #[test]
    fn output_reader_notifies_when_silent_child_exits() {
        let (notify_tx, notify_rx) = mpsc::channel();
        let mut pty = Pty::spawn_notified_with_args(
            "/bin/sh",
            ["-c", "exit 23"],
            Path::new("/"),
            80,
            24,
            move || {
                let _ = notify_tx.send(());
            },
        )
        .unwrap();
        notify_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("EOF should wake the event loop");
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = pty.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "silent child did not exit");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.exit_code(), 23);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn drop_terminates_child() {
        let pty = Pty::spawn("/bin/sh", Path::new("/"), 80, 24).unwrap();
        let pid = pty.process_id().unwrap();
        drop(pty);
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "child {pid} remains after drop"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn setup_failure_kills_and_reaps_spawned_child() {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new("/bin/sleep");
        command.arg("100");
        let child = pair.slave.spawn_command(command).unwrap();
        let pid = child.process_id().unwrap();
        let session_id = pty_session_id(child.as_ref());
        let error = setup_spawned_child(child, session_id, || {
            Err::<(), _>(io::Error::other("injected post-spawn setup failure"))
        })
        .expect_err("injected setup error is returned");
        assert_eq!(error.to_string(), "injected post-spawn setup failure");
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "spawned child {pid} survived setup rollback"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn drop_terminates_shell_and_background_process_group_promptly() {
        const CHILD_TEST: &str = "SO1TERM_PTY_DROP_TEST_CHILD";
        if std::env::var_os(CHILD_TEST).is_some() {
            assert_dropped_sleep_is_gone();
            return;
        }

        let mut helper = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "pty::tests::drop_terminates_shell_and_background_process_group_promptly",
            ])
            .env(CHILD_TEST, "1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(status) = helper.try_wait().unwrap() {
                assert!(status.success(), "PTY drop helper failed with {status}");
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = helper.kill();
                let _ = helper.wait();
                panic!("PTY drop helper did not finish within 4 seconds");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn drop_terminates_so1sh_started_background_process() {
        let Some(executable) = test_so1sh_executable() else {
            eprintln!("skip real so1sh background teardown test: Cargo binary unavailable");
            return;
        };
        let unique = format!("so1term-pty-{}", std::process::id());
        let rc_path = std::env::temp_dir().join(format!("{unique}.rc"));
        let pid_path = std::env::temp_dir().join(format!("{unique}.pid"));
        std::fs::write(
            &rc_path,
            format!("sleep 100 & echo CHILD_PID=$! > '{}'\n", pid_path.display()),
        )
        .unwrap();
        let command = format!(
            "SO1SH_RC='{}' exec '{}'",
            rc_path.display(),
            executable.display()
        );
        let pty = Pty::spawn_with_args("/bin/sh", ["-c", command.as_str()], "/", 80, 24).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Ok(value) = std::fs::read_to_string(&pid_path)
                && let Some(pid) = value
                    .trim()
                    .strip_prefix("CHILD_PID=")
                    .and_then(|s| s.parse::<u32>().ok())
            {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "so1sh rc did not launch background child"
            );
            thread::sleep(Duration::from_millis(10));
        };
        let started = std::time::Instant::now();
        drop(pty);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "so1sh PTY drop took {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_file(rc_path);
        let _ = std::fs::remove_file(pid_path);
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "so1sh background process {pid} survived drop"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn drop_sweeps_background_job_after_shell_was_reaped() {
        let executable =
            test_so1sh_executable().expect("so1sh binary is required for teardown regression");
        let unique = format!("so1term-reaped-{}", std::process::id());
        let rc_path = std::env::temp_dir().join(format!("{unique}.rc"));
        let pid_path = std::env::temp_dir().join(format!("{unique}.pid"));
        std::fs::write(
            &rc_path,
            format!(
                "sh -c 'echo $$ > {}; exec sleep 100' &\n",
                pid_path.display()
            ),
        )
        .unwrap();
        let command = format!(
            "SO1SH_RC='{}' exec '{}'",
            rc_path.display(),
            executable.display()
        );
        let mut pty =
            Pty::spawn_with_args("/bin/sh", ["-c", command.as_str()], "/", 80, 24).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let child_pid = loop {
            if let Ok(value) = std::fs::read_to_string(&pid_path)
                && let Ok(pid) = value.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "background job did not publish its pid"
            );
            thread::sleep(Duration::from_millis(10));
        };
        pty.write(b"exit\n").unwrap();
        while pty.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "so1sh did not exit");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            Path::new(&format!("/proc/{child_pid}")).exists(),
            "background process exited before teardown"
        );
        let command_line = std::fs::read(format!("/proc/{child_pid}/cmdline")).unwrap();
        let arguments: Vec<_> = command_line.split(|byte| *byte == 0).collect();
        assert!(
            arguments.iter().any(|argument| *argument == b"sleep")
                && arguments.iter().any(|argument| *argument == b"100"),
            "pid file did not identify the expected sleep process: {command_line:?}"
        );
        let started = Instant::now();
        drop(pty);
        let _ = std::fs::remove_file(rc_path);
        let _ = std::fs::remove_file(pid_path);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            wait_for_pid_gone_or_zombie(
                child_pid as libc::pid_t,
                Instant::now() + Duration::from_secs(2)
            ),
            "reaped shell's background process {child_pid} remained runnable after teardown"
        );
    }

    #[test]
    fn group_signal_requires_a_live_member_in_the_recorded_session() {
        let members = [(10, 10, 10), (11, 11, 99), (12, 10, 88)];
        assert!(group_has_session_member(10, 10, &members));
        assert!(!group_has_session_member(10, 11, &members));
        assert!(!group_has_session_member(99, 10, &members));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_stat_state_parser_distinguishes_zombie_from_live() {
        assert_eq!(process_state_from_stat("123 (sleep) S 1 2 3"), Some('S'));
        assert_eq!(
            process_state_from_stat("123 (sleep weird) Z 1 2 3"),
            Some('Z')
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn teardown_preserves_unrelated_session_process_group() {
        let mut unrelated = std::process::Command::new("setsid")
            .args(["/bin/sleep", "100"])
            .spawn()
            .expect("setsid is available on Linux");
        let pid = unrelated.id();
        let pty = Pty::spawn_with_args("/bin/sh", ["-c", "sleep 100 & wait"], "/", 80, 24).unwrap();
        drop(pty);
        assert!(
            Path::new(&format!("/proc/{pid}")).exists(),
            "unrelated session process {pid} was killed"
        );
        unrelated.kill().unwrap();
        let _ = unrelated.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_group_signal_rejects_a_group_from_another_session() {
        let mut unrelated = std::process::Command::new("setsid")
            .args(["/bin/sleep", "100"])
            .spawn()
            .expect("setsid is available on Linux");
        let unrelated_pid = unrelated.id() as libc::pid_t;
        let unrelated_session = unsafe { libc::getsid(unrelated_pid) };
        let own = Pty::spawn_with_args("/bin/sleep", ["100"], "/", 80, 24).unwrap();
        let own_session = own.session_id.expect("PTY session captured at spawn");
        assert_ne!(own_session, unrelated_session);

        signal_session_group(own_session, unrelated_pid, libc::SIGKILL);
        assert!(
            Path::new(&format!("/proc/{unrelated_pid}")).exists(),
            "foreign process group was signalled"
        );
        unrelated.kill().unwrap();
        let _ = unrelated.wait();
        drop(own);
    }

    #[cfg(target_os = "linux")]
    fn assert_dropped_sleep_is_gone() {
        let mut pty = Pty::spawn_with_args(
            "/bin/sh",
            ["-c", "sleep 100 & printf 'CHILD_PID=%s\\n' \"$!\"; wait"],
            Path::new("/"),
            80,
            24,
        )
        .unwrap();
        let output = wait_for(&mut pty, b"CHILD_PID=");
        let pid = String::from_utf8_lossy(&output)
            .split("CHILD_PID=")
            .nth(1)
            .and_then(|tail| tail.lines().next())
            .and_then(|value| value.trim().parse::<u32>().ok())
            .expect("shell reports background PID");
        let started = std::time::Instant::now();
        drop(pty);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "drop took {:?}",
            started.elapsed()
        );
        assert!(
            wait_for_pid_gone_or_zombie(
                pid as libc::pid_t,
                Instant::now() + Duration::from_secs(2)
            ),
            "background process {pid} remained runnable after teardown"
        );
    }
}
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
};

#[cfg(test)]
pub(crate) fn test_so1sh_executable() -> Option<PathBuf> {
    std::env::var_os("CARGO_BIN_EXE_so1sh")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            let executable = std::env::current_exe().ok()?;
            executable
                .ancestors()
                .filter_map(|ancestor| {
                    let profile = if ancestor.file_name().is_some_and(|name| name == "build") {
                        ancestor.parent()?
                    } else if ancestor.join("deps").is_dir() {
                        ancestor
                    } else {
                        return None;
                    };
                    let candidate = profile.join("so1sh");
                    candidate.is_file().then_some(candidate)
                })
                .next()
        })
}

/// A headless child process attached to a portable pseudo-terminal.
pub struct Pty {
    child: Box<dyn Child + Send + Sync>,
    session_id: Option<i32>,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    output: Receiver<Vec<u8>>,
    reader: Option<JoinHandle<()>>,
}

impl Pty {
    /// Spawn an executable in a PTY. `size` is supplied as columns then rows.
    pub fn spawn(
        executable: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
        cols: u16,
        rows: u16,
    ) -> io::Result<Self> {
        Self::spawn_with_args(executable, std::iter::empty::<&str>(), cwd, cols, rows)
    }

    /// Spawn an executable with arguments in a PTY.
    pub fn spawn_with_args<I, S>(
        executable: impl AsRef<Path>,
        args: I,
        cwd: impl AsRef<Path>,
        cols: u16,
        rows: u16,
    ) -> io::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let system = native_pty_system();
        let pair = system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(other_io)?;
        let mut command = CommandBuilder::new(executable.as_ref().as_os_str());
        command.args(args);
        command.cwd(cwd.as_ref().as_os_str());
        command.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(command).map_err(other_io)?;
        let session_id = pty_session_id(child.as_ref());
        let (child, (writer, output, reader)) = setup_spawned_child(child, session_id, || {
            let reader = pair.master.try_clone_reader().map_err(other_io)?;
            let writer = pair.master.take_writer().map_err(other_io)?;
            let (sender, output) = mpsc::channel();
            let reader = thread::Builder::new()
                .name("so1term-pty-reader".into())
                .spawn(move || read_output(reader, sender))?;
            Ok((writer, output, reader))
        })?;
        Ok(Self {
            child,
            session_id,
            master: Some(pair.master),
            writer: Some(writer),
            output,
            reader: Some(reader),
        })
    }

    /// Spawn the configured shell, preferring a sibling `so1sh` on Unix.
    pub fn spawn_shell(cwd: impl AsRef<Path>, cols: u16, rows: u16) -> io::Result<Self> {
        Self::spawn_shell_notified(cwd, cols, rows, || {})
    }

    /// Spawn the shell and notify the UI after every output chunk is queued.
    pub fn spawn_shell_notified<F>(
        cwd: impl AsRef<Path>,
        cols: u16,
        rows: u16,
        notify: F,
    ) -> io::Result<Self>
    where
        F: Fn() + Send + Sync + 'static,
    {
        let notify = std::sync::Arc::new(notify);
        if let Some(override_path) = std::env::var_os("SO1TERM_SHELL") {
            let callback = std::sync::Arc::clone(&notify);
            return Self::spawn_notified(override_path, cwd, cols, rows, move || callback());
        }
        #[cfg(unix)]
        {
            let callback = std::sync::Arc::clone(&notify);
            Self::spawn_notified(resolve_shell(), cwd, cols, rows, move || callback())
        }
        #[cfg(windows)]
        {
            let candidates = crate::default_shell_candidates_for(true);
            let mut last_error = None;
            for candidate in candidates {
                let callback = std::sync::Arc::clone(&notify);
                match Self::spawn_notified(candidate, cwd.as_ref(), cols, rows, move || callback())
                {
                    Ok(pty) => return Ok(pty),
                    Err(error) => last_error = Some(error),
                }
            }
            Err(last_error
                .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no shell candidate")))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (cwd, cols, rows);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "PTY shell unsupported on this platform",
            ))
        }
    }

    fn spawn_notified(
        executable: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
        cols: u16,
        rows: u16,
        notify: impl Fn() + Send + 'static,
    ) -> io::Result<Self> {
        Self::spawn_notified_with_args(
            executable,
            std::iter::empty::<&str>(),
            cwd,
            cols,
            rows,
            notify,
        )
    }

    fn spawn_notified_with_args<I, S>(
        executable: impl AsRef<Path>,
        args: I,
        cwd: impl AsRef<Path>,
        cols: u16,
        rows: u16,
        notify: impl Fn() + Send + 'static,
    ) -> io::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let system = native_pty_system();
        let pair = system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(other_io)?;
        let mut command = CommandBuilder::new(executable.as_ref().as_os_str());
        command.args(args);
        command.cwd(cwd.as_ref().as_os_str());
        command.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(command).map_err(other_io)?;
        let session_id = pty_session_id(child.as_ref());
        let (child, (writer, output, reader)) = setup_spawned_child(child, session_id, || {
            let reader = pair.master.try_clone_reader().map_err(other_io)?;
            let writer = pair.master.take_writer().map_err(other_io)?;
            let (sender, output) = mpsc::channel();
            let reader = thread::Builder::new()
                .name("so1term-pty-reader".into())
                .spawn(move || read_output_notified(reader, sender, notify))?;
            Ok((writer, output, reader))
        })?;
        Ok(Self {
            child,
            session_id,
            master: Some(pair.master),
            writer: Some(writer),
            output,
            reader: Some(reader),
        })
    }

    /// Send raw terminal bytes to the child.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "PTY is closed"))?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    /// Update PTY dimensions in columns, rows, pixel width and pixel height.
    pub fn resize(
        &self,
        cols: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> io::Result<()> {
        self.master
            .as_ref()
            .expect("master exists until drop")
            .resize(PtySize {
                rows,
                cols,
                pixel_width,
                pixel_height,
            })
            .map_err(other_io)
    }

    /// Receive a single available output chunk without blocking.
    pub fn try_read(&self) -> Option<Vec<u8>> {
        self.output.try_recv().ok()
    }

    /// Drain all currently queued output chunks.
    pub fn drain_output(&self) -> Vec<Vec<u8>> {
        self.output.try_iter().collect()
    }

    /// Poll the child's exit status without blocking.
    pub fn try_wait(&mut self) -> io::Result<Option<portable_pty::ExitStatus>> {
        self.child.try_wait()
    }

    /// Return the child process id when available.
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let foreground_group = self
            .master
            .as_ref()
            .and_then(|master| master.process_group_leader())
            .map(|pgid| pgid as u32);
        terminate_child_session(self.child.as_mut(), self.session_id, foreground_group);
        self.writer.take();
        self.master.take();
        if let Some(reader) = self.reader.take() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
            while !reader.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if reader.is_finished() {
                let _ = reader.join();
            } else {
                // A reader blocked in the PTY driver must not hold UI shutdown hostage.
                eprintln!(
                    "PTY output reader did not stop within 100ms; detaching it during teardown"
                );
            }
        }
    }
}

fn setup_spawned_child<T>(
    child: Box<dyn Child + Send + Sync>,
    session_id: Option<i32>,
    setup: impl FnOnce() -> io::Result<T>,
) -> io::Result<(Box<dyn Child + Send + Sync>, T)> {
    let mut guard = SpawnedChild {
        child: Some(child),
        session_id,
    };
    let resources = setup()?;
    Ok((
        guard.child.take().expect("spawn guard contains child"),
        resources,
    ))
}

struct SpawnedChild {
    child: Option<Box<dyn Child + Send + Sync>>,
    session_id: Option<i32>,
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            terminate_child_session(child.as_mut(), self.session_id, None);
        }
    }
}

fn terminate_child_session(
    child: &mut dyn Child,
    session_id: Option<i32>,
    extra_group: Option<u32>,
) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(session) = session_id {
        let mut groups = session_process_groups(session);
        if let Some(group) = extra_group.map(|group| group as libc::pid_t) {
            groups.push(group);
        }
        groups.sort_unstable();
        groups.dedup();
        for group in &groups {
            signal_session_group(session, *group, libc::SIGHUP);
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(150);
        while std::time::Instant::now() < deadline {
            if session_process_groups(session).is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        groups.extend(session_process_groups(session));
        groups.sort_unstable();
        groups.dedup();
        for group in groups {
            signal_session_group(session, group, libc::SIGKILL);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = (session_id, extra_group);

    match child.try_wait() {
        Ok(Some(_)) | Err(_) => {
            // A reaped child may have had its PID reused; an unknown wait state
            // is not authorization to signal that PID either.
        }
        Ok(None) => {
            #[cfg(unix)]
            {
                let pid = child.process_id().map(|pid| pid as libc::pid_t);
                let same_session = pid
                    .zip(session_id)
                    .is_some_and(|(pid, session)| unsafe { libc::getsid(pid) == session });
                if same_session {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn pty_session_id(child: &dyn Child) -> Option<i32> {
    #[cfg(unix)]
    {
        let pid = child.process_id()? as libc::pid_t;
        let session = unsafe { libc::getsid(pid) };
        (session > 0).then_some(session)
    }
    #[cfg(not(unix))]
    {
        let _ = child;
        None
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn group_has_session_member(
    session: libc::pid_t,
    group: libc::pid_t,
    members: &[(libc::pid_t, libc::pid_t, libc::pid_t)],
) -> bool {
    members
        .iter()
        .any(|&(pid, pgid, sid)| pid > 0 && pgid == group && sid == session)
}

#[cfg(target_os = "macos")]
fn session_process_members(session: libc::pid_t) -> Vec<(libc::pid_t, libc::pid_t)> {
    let mut pids = vec![0 as libc::pid_t; 16_384];
    let count = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if count <= 0 {
        return Vec::new();
    }
    pids.truncate((count as usize).min(pids.len()));
    pids.into_iter()
        .filter_map(|pid| {
            if pid <= 0 || unsafe { libc::getsid(pid) } != session {
                return None;
            }
            let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
            let copied = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    (&mut info as *mut libc::proc_bsdinfo).cast(),
                    std::mem::size_of_val(&info) as libc::c_int,
                )
            };
            if copied != std::mem::size_of_val(&info) as libc::c_int
                || info.pbi_status == libc::SZOMB
            {
                return None;
            }
            let group = info.pbi_pgid as libc::pid_t;
            (group > 0 && unsafe { libc::getpgid(pid) } == group).then_some((pid, group))
        })
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn signal_session_group(session: libc::pid_t, group: libc::pid_t, signal: libc::c_int) {
    let verified_members: Vec<_> = session_process_members(session)
        .into_iter()
        .filter(|&(pid, pgid)| unsafe {
            libc::getsid(pid) == session && libc::getpgid(pid) == pgid
        })
        .map(|(pid, pgid)| (pid, pgid, unsafe { libc::getsid(pid) }))
        .collect();
    let valid_member = group_has_session_member(session, group, &verified_members);
    if valid_member {
        unsafe {
            libc::kill(-group, signal);
        }
    }
}

#[cfg(target_os = "linux")]
fn session_process_members(session: libc::pid_t) -> Vec<(libc::pid_t, libc::pid_t)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .parse::<libc::pid_t>()
                .ok()
        })
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let after_command = stat.rsplit_once(") ")?.1;
            let fields: Vec<_> = after_command.split_whitespace().collect();
            if matches!(process_state_from_stat(&stat), Some('Z' | 'X')) {
                return None;
            }
            let process_group = fields.get(2)?.parse::<libc::pid_t>().ok()?;
            let process_session = fields.get(3)?.parse::<libc::pid_t>().ok()?;
            (process_session == session).then_some((pid, process_group))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn process_state_from_stat(stat: &str) -> Option<char> {
    stat.rsplit_once(") ")?.1.chars().next()
}

#[cfg(all(target_os = "linux", test))]
fn wait_for_pid_gone_or_zombie(pid: libc::pid_t, deadline: std::time::Instant) -> bool {
    loop {
        let path = format!("/proc/{pid}/stat");
        match std::fs::read_to_string(path) {
            Ok(stat) if matches!(process_state_from_stat(&stat), Some('Z' | 'X')) => return true,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return true,
            Err(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn session_process_groups(session: libc::pid_t) -> Vec<libc::pid_t> {
    session_process_members(session)
        .into_iter()
        .map(|(_, group)| group)
        .collect()
}

fn read_output(mut reader: Box<dyn Read + Send>, sender: mpsc::Sender<Vec<u8>>) {
    let mut buffer = [0u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(length) => {
                if sender.send(buffer[..length].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

fn read_output_notified(
    mut reader: Box<dyn Read + Send>,
    sender: mpsc::Sender<Vec<u8>>,
    notify: impl Fn(),
) {
    let mut buffer = [0u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => {
                notify();
                break;
            }
            Ok(length) => {
                if sender.send(buffer[..length].to_vec()).is_err() {
                    break;
                }
                notify();
            }
        }
    }
}

fn other_io(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

/// Resolve the default so1sh executable, preferring a sibling to PATH.
#[cfg(unix)]
pub fn resolve_shell() -> PathBuf {
    if let Some(value) = std::env::var_os("SO1TERM_SHELL") {
        return PathBuf::from(value);
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join("so1sh")));
    match crate::resolve_shell_choice(
        None,
        sibling.as_ref().is_some_and(|p| p.is_file()),
        crate::default_shell_candidates_for(false),
    ) {
        "sibling-so1sh" => sibling.unwrap_or_else(|| PathBuf::from("so1sh")),
        selected => PathBuf::from(selected),
    }
}

#[cfg(test)]
mod notification_tests {
    use super::read_output_notified;
    use std::{
        io::{self, Cursor, Read},
        sync::mpsc,
    };

    struct FailingReader;
    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic reader failure"))
        }
    }

    #[test]
    fn reader_notifies_after_data_and_after_eof_or_error() {
        let (events, received) = mpsc::channel();
        let (output, chunks) = mpsc::channel();
        read_output_notified(Box::new(Cursor::new(b"data")), output, || {
            events.send(()).unwrap()
        });
        assert_eq!(chunks.try_iter().collect::<Vec<_>>(), [b"data".to_vec()]);
        assert_eq!(
            received.try_iter().count(),
            2,
            "data and EOF each wake the event loop"
        );

        let (events, received) = mpsc::channel();
        let (output, _chunks) = mpsc::channel();
        read_output_notified(Box::new(FailingReader), output, || events.send(()).unwrap());
        assert_eq!(
            received.try_iter().count(),
            1,
            "read errors wake the event loop"
        );
    }
}

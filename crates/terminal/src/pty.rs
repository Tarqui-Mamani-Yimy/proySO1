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
}
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
};

/// A headless child process attached to a portable pseudo-terminal.
pub struct Pty {
    child: Box<dyn Child + Send + Sync>,
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
        let reader = pair.master.try_clone_reader().map_err(other_io)?;
        let writer = pair.master.take_writer().map_err(other_io)?;
        let (sender, output) = mpsc::channel();
        let reader = thread::Builder::new()
            .name("so1term-pty-reader".into())
            .spawn(move || read_output(reader, sender))?;
        Ok(Self {
            child,
            master: Some(pair.master),
            writer: Some(writer),
            output,
            reader: Some(reader),
        })
    }

    /// Spawn the configured shell, preferring a sibling `so1sh` on Unix.
    pub fn spawn_shell(cwd: impl AsRef<Path>, cols: u16, rows: u16) -> io::Result<Self> {
        if let Some(override_path) = std::env::var_os("SO1TERM_SHELL") {
            return Self::spawn(override_path, cwd, cols, rows);
        }
        #[cfg(unix)]
        {
            Self::spawn(resolve_shell(), cwd, cols, rows)
        }
        #[cfg(windows)]
        {
            let candidates = crate::default_shell_candidates_for(true);
            let mut last_error = None;
            for candidate in candidates {
                match Self::spawn(candidate, cwd.as_ref(), cols, rows) {
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
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        self.writer.take();
        self.master.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
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

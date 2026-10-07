use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const FOREGROUND_WAIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
const FOREGROUND_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Acquired foreground ownership and cooked modes, independent of redirected stdin.
pub struct Terminal {
    file: File,
    modes: libc::termios,
    pgid: libc::pid_t,
    original_group: libc::pid_t,
}

/// Process-wide prompt policy. Evaluation temporarily catches INT/QUIT in epsh.
pub fn ignore_prompt_signals() -> io::Result<()> {
    // SAFETY: initialized target-specific actions, used only by this binary.
    unsafe {
        for signal in [
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTSTP,
            libc::SIGTTIN,
            libc::SIGTTOU,
        ] {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_IGN;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) == -1 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

impl Terminal {
    /// Wait for foreground permission with SIGTTIN before taking the terminal.
    /// If stops are discarded (e.g. an orphaned group), back off and time out.
    /// Changes this process's group and prompt signal dispositions; Drop returns
    /// terminal ownership to the group observed before acquisition.
    pub fn acquire() -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        // SAFETY: live tty fd; job-control operations target our own group.
        unsafe {
            let original_group = libc::tcgetpgrp(file.as_raw_fd());
            if original_group == -1 {
                return Err(io::Error::last_os_error());
            }
            // A parent may have ignored or blocked TTIN; restore stop semantics
            // before waiting, never steal foreground ownership while backgrounded.
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut action.sa_mask);
            let mut unblock: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut unblock);
            libc::sigaddset(&mut unblock, libc::SIGTTIN);
            if libc::sigaction(libc::SIGTTIN, &action, std::ptr::null_mut()) == -1
                || libc::sigprocmask(libc::SIG_UNBLOCK, &unblock, std::ptr::null_mut()) == -1
            {
                return Err(io::Error::last_os_error());
            }
            let mut deadline = std::time::Instant::now() + FOREGROUND_WAIT_TIMEOUT;
            loop {
                let foreground = libc::tcgetpgrp(file.as_raw_fd());
                if foreground == -1 {
                    return Err(io::Error::last_os_error());
                }
                let group = libc::getpgrp();
                if foreground == group {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "foreground permission unavailable (possibly orphaned process group)",
                    ));
                }
                if libc::kill(-group, libc::SIGTTIN) == -1 {
                    return Err(io::Error::last_os_error());
                }
                // A normal stop suspends us until resumed. An orphan's stop is
                // discarded, so sleep to avoid a tight signal/syscall loop.
                let sleep_started = std::time::Instant::now();
                std::thread::sleep(FOREGROUND_WAIT_INTERVAL);
                let elapsed = sleep_started.elapsed();
                // A delivered SIGTTIN suspends this thread until SIGCONT. Do
                // not charge that stopped interval against the fallback timer.
                deadline += elapsed.saturating_sub(FOREGROUND_WAIT_INTERVAL);
            }
            let pgid = libc::getpid();
            if libc::getpgrp() != pgid && libc::setpgid(0, pgid) == -1 {
                return Err(io::Error::last_os_error());
            }
            ignore_prompt_signals()?;
            if libc::tcsetpgrp(file.as_raw_fd(), pgid) == -1 {
                return Err(io::Error::last_os_error());
            }
            let mut modes = std::mem::zeroed();
            if libc::tcgetattr(file.as_raw_fd(), &mut modes) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                file,
                modes,
                pgid,
                original_group,
            })
        }
    }

    pub fn reclaim(&self) -> io::Result<()> {
        // SAFETY: saved group and modes belong to this shell's controlling tty.
        unsafe {
            if libc::tcsetpgrp(self.file.as_raw_fd(), self.pgid) == -1
                || libc::tcsetattr(self.file.as_raw_fd(), libc::TCSADRAIN, &self.modes) == -1
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Write shell-integration metadata directly to the controlling terminal.
    pub fn write_shell_integration(&self, bytes: &[u8]) -> io::Result<()> {
        let mut file = &self.file;
        file.write_all(bytes)
    }

    /// Reedline renders prompts to stdout; only enable its prompt markers when
    /// stdout is the same terminal acquired for interactive input.
    pub fn stdout_is_controlling_tty(&self) -> bool {
        unsafe {
            libc::isatty(libc::STDOUT_FILENO) == 1
                && libc::tcgetpgrp(libc::STDOUT_FILENO) == self.pgid
                && libc::tcgetpgrp(self.file.as_raw_fd()) == self.pgid
        }
    }

    /// Report the cwd as an OSC 7 file URL without routing through shell output.
    pub fn report_cwd(&self, cwd: &Path) -> io::Result<()> {
        let hostname = hostname_bytes()?;
        let mut sequence = b"\x1b]7;file://".to_vec();
        append_url_bytes(&mut sequence, &hostname, false);
        append_url_bytes(&mut sequence, cwd.as_os_str().as_bytes(), true);
        sequence.extend_from_slice(b"\x1b\\");
        self.write_shell_integration(&sequence)
    }
}

fn hostname_bytes() -> io::Result<Vec<u8>> {
    let mut buffer = [0u8; 256];
    // SAFETY: buffer is writable and its length is passed to gethostname.
    let hostname = if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
        let len = buffer
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(buffer.len());
        Some(buffer[..len].to_vec())
    } else {
        None
    };
    let nodename = uname_nodename_bytes();
    select_hostname(hostname, nodename)
        .ok_or_else(|| io::Error::other("could not determine the local hostname"))
}

fn uname_nodename_bytes() -> Option<Vec<u8>> {
    // SAFETY: utsname is zero-initialized and libc::uname fills it on success.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut name) } == -1 {
        return None;
    }
    // SAFETY: nodename is a fixed-size C string field inside the initialized
    // utsname; bound the scan to that field even if it lacks a trailing NUL.
    let nodename = unsafe {
        std::slice::from_raw_parts(name.nodename.as_ptr().cast::<u8>(), name.nodename.len())
    };
    let len = nodename
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(nodename.len());
    Some(nodename[..len].to_vec())
}

fn select_hostname(hostname: Option<Vec<u8>>, nodename: Option<Vec<u8>>) -> Option<Vec<u8>> {
    hostname
        .filter(|name| !name.is_empty())
        .or_else(|| nodename.filter(|name| !name.is_empty()))
}

fn append_url_bytes(out: &mut Vec<u8>, bytes: &[u8], keep_slash: bool) {
    for &byte in bytes {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~')
            || (keep_slash && byte == b'/')
        {
            out.push(byte);
        } else {
            out.extend_from_slice(format!("%{byte:02X}").as_bytes());
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: TTIN/TTOU remain ignored; return the tty to its original owner.
        unsafe {
            libc::tcsetattr(self.file.as_raw_fd(), libc::TCSADRAIN, &self.modes);
            libc::tcsetpgrp(self.file.as_raw_fd(), self.original_group);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{append_url_bytes, select_hostname};

    #[test]
    fn osc7_url_bytes_are_percent_encoded_without_losing_path_slashes() {
        let mut url = Vec::new();
        append_url_bytes(&mut url, b"host name", false);
        append_url_bytes(&mut url, b"/tmp/a b%", true);
        assert_eq!(url, b"host%20name/tmp/a%20b%25");
    }

    #[test]
    fn hostname_selection_prefers_gethostname_then_uname_nodename() {
        assert_eq!(
            select_hostname(Some(b"host-name".to_vec()), Some(b"uname-name".to_vec())).unwrap(),
            b"host-name"
        );
        assert_eq!(
            select_hostname(None, Some(b"uname-name".to_vec())).unwrap(),
            b"uname-name"
        );
        assert!(select_hostname(None, None).is_none());
    }
}

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;

/// Interactive-only ownership and cooked modes, independent of redirected stdin.
pub struct Terminal {
    file: File,
    modes: libc::termios,
    pgid: libc::pid_t,
}

impl Terminal {
    pub fn new() -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        // SAFETY: live controlling-terminal fd; signal actions and termios are
        // initialized with libc's target-specific types. Runs only in this binary.
        unsafe {
            for sig in [
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTSTP,
                libc::SIGTTIN,
                libc::SIGTTOU,
            ] {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_IGN;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(sig, &action, std::ptr::null_mut()) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            let pgid = libc::getpid();
            if libc::getpgrp() != pgid && libc::setpgid(0, pgid) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::tcsetpgrp(file.as_raw_fd(), pgid) == -1 {
                return Err(io::Error::last_os_error());
            }
            let mut modes = std::mem::zeroed();
            if libc::tcgetattr(file.as_raw_fd(), &mut modes) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { file, modes, pgid })
        }
    }

    pub fn restore(&self) -> io::Result<()> {
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
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

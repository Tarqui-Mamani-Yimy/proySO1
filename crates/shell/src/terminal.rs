use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;

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

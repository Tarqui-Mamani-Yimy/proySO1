//! Signal handling for trap execution.
//!
//! When a trap is set for a signal (e.g. `trap 'cleanup' INT`), we install a
//! signal handler that sets a per-signal atomic flag. The shell checks these
//! flags after every command completes and runs the corresponding trap action
//! before the next command, as dash does. A foreground command is never
//! interrupted (POSIX defers the trap until it completes); the `wait` builtin is,
//! through a self-pipe the handler writes to while `wait` is blocked.
//!
//! Signal numbers are rustix `Signal` values (no libc constants). Handler
//! installation registers a raw C-style handler pointer via `sigaction`
//! (avoiding rustix's `kernel_sigaction` trampoline, which is unsafe under
//! static-musl linking); reset/ignore use rustix `kernel_sigaction` with the
//! `SIG_DFL`/`SIG_IGN` kernel handlers (no trampoline involved).

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use rustix::process::Signal;

/// One more than the highest signal number tracked for traps.
const SIGNAL_SLOTS: usize = 65;

/// Per-signal "trap pending" flags, indexed by signal number.
static PENDING: [AtomicBool; SIGNAL_SLOTS] = [const { AtomicBool::new(false) }; SIGNAL_SLOTS];
/// Set whenever any `PENDING` flag is set, so the per-command check is one load.
static ANY_PENDING: AtomicBool = AtomicBool::new(false);
static INTERACTIVE_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn interactive_handler(signal: i32) {
    INTERACTIVE_SIGNAL.store(signal, Ordering::Relaxed);
}

/// Evaluation-only signal policy; restores the caller's dispositions and mask.
/// Interactive shells opt in, never non-interactive embedding applications.
pub(crate) struct InteractiveSignalScope {
    actions: Vec<(i32, libc::sigaction)>,
    mask: Option<libc::sigset_t>,
}

impl InteractiveSignalScope {
    pub(crate) fn install() -> std::io::Result<Self> {
        let mut scope = Self {
            actions: Vec::new(),
            mask: None,
        };
        INTERACTIVE_SIGNAL.store(0, Ordering::Relaxed);
        // SAFETY: initialized target-specific storage; invoked before evaluation,
        // not from a handler. Drop rolls back even partially installed actions.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = interactive_handler as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            let mut unblock: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut unblock);
            for signal in [libc::SIGINT, libc::SIGQUIT] {
                let mut old = std::mem::zeroed();
                if libc::sigaction(signal, &action, &mut old) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                scope.actions.push((signal, old));
                libc::sigaddset(&mut unblock, signal);
            }
            let mut old_mask = std::mem::zeroed();
            if libc::sigprocmask(libc::SIG_UNBLOCK, &unblock, &mut old_mask) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            scope.mask = Some(old_mask);
        }
        Ok(scope)
    }
}

impl Drop for InteractiveSignalScope {
    fn drop(&mut self) {
        // SAFETY: these actions and mask were saved from this process/thread.
        unsafe {
            for (signal, action) in &self.actions {
                libc::sigaction(*signal, action, std::ptr::null_mut());
            }
            if let Some(mask) = &self.mask {
                libc::sigprocmask(libc::SIG_SETMASK, mask, std::ptr::null_mut());
            }
        }
        INTERACTIVE_SIGNAL.store(0, Ordering::Relaxed);
    }
}

pub(crate) fn take_interactive_signal() -> i32 {
    INTERACTIVE_SIGNAL.swap(0, Ordering::Relaxed)
}

/// Map a signal name (e.g. "INT", "SIGINT") to its `Signal`.
pub fn name_to_signal(name: &str) -> Option<Signal> {
    // Strip "SIG" prefix if present
    let name = name.strip_prefix("SIG").unwrap_or(name);
    match name {
        "HUP" => Some(Signal::HUP),
        "INT" => Some(Signal::INT),
        "QUIT" => Some(Signal::QUIT),
        "ILL" => Some(Signal::ILL),
        "TRAP" => Some(Signal::TRAP),
        "ABRT" | "IOT" => Some(Signal::ABORT),
        "FPE" => Some(Signal::FPE),
        "KILL" => Some(Signal::KILL),
        "BUS" => Some(Signal::BUS),
        "SEGV" => Some(Signal::SEGV),
        "SYS" => Some(Signal::SYS),
        "PIPE" => Some(Signal::PIPE),
        "ALRM" => Some(Signal::ALARM),
        "TERM" => Some(Signal::TERM),
        "URG" => Some(Signal::URG),
        "STOP" => Some(Signal::STOP),
        "TSTP" => Some(Signal::TSTP),
        "CONT" => Some(Signal::CONT),
        "CHLD" => Some(Signal::CHILD),
        "TTIN" => Some(Signal::TTIN),
        "TTOU" => Some(Signal::TTOU),
        "USR1" => Some(Signal::USR1),
        "USR2" => Some(Signal::USR2),
        _ => None,
    }
}

/// Map a signal number to its name (without SIG prefix).
pub fn signal_to_name(signum: i32) -> Option<&'static str> {
    let sig = Signal::from_named_raw(signum)?;
    let name = match sig {
        Signal::HUP => "HUP",
        Signal::INT => "INT",
        Signal::QUIT => "QUIT",
        Signal::ILL => "ILL",
        Signal::TRAP => "TRAP",
        Signal::ABORT => "ABRT",
        Signal::FPE => "FPE",
        Signal::KILL => "KILL",
        Signal::BUS => "BUS",
        Signal::SEGV => "SEGV",
        Signal::SYS => "SYS",
        Signal::PIPE => "PIPE",
        Signal::ALARM => "ALRM",
        Signal::TERM => "TERM",
        Signal::URG => "URG",
        Signal::STOP => "STOP",
        Signal::TSTP => "TSTP",
        Signal::CONT => "CONT",
        Signal::CHILD => "CHLD",
        Signal::TTIN => "TTIN",
        Signal::TTOU => "TTOU",
        Signal::USR1 => "USR1",
        Signal::USR2 => "USR2",
        _ => return None,
    };
    Some(name)
}

/// Install a signal handler that sets the pending flag for the given signal.
/// Call this when a trap is set for a signal.
pub fn install_handler(sig: Signal) {
    // SAFETY: sigaction is async-signal-safe. The handler only sets an atomic
    // flag. We register a raw C-style handler pointer (SA_RESTART) rather than
    // rustix's trampoline-based kernel_sigaction, which is unsafe under static
    // musl linking.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = signal_handler as *const () as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig.as_raw(), &sa, std::ptr::null_mut());
        }
        #[cfg(target_os = "macos")]
        {
            darwin_sigaction(sig.as_raw(), signal_handler as *const () as usize, 0x0002);
        }
    }
}

/// Reset a signal to its default disposition.
/// Call this when a trap is removed for a signal.
pub fn reset_handler(sig: Signal) {
    // SAFETY: kernel_sigaction with KERNEL_SIG_DFL is always safe.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            let action = rustix::runtime::KernelSigaction {
                sa_handler_kernel: rustix::runtime::KERNEL_SIG_DFL,
                sa_flags: rustix::runtime::KernelSigactionFlags::empty(),
                sa_restorer: None,
                sa_mask: rustix::runtime::KernelSigSet::empty(),
            };
            let _ = rustix::runtime::kernel_sigaction(sig, Some(action));
        }
        #[cfg(target_os = "macos")]
        {
            darwin_sigaction(sig.as_raw(), 0, 0);
        }
    }
}

/// Set a signal to be ignored (for `trap '' SIG`).
pub fn ignore_signal(sig: Signal) {
    // SAFETY: kernel_sigaction with kernel_sig_ign() is always safe.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            let action = rustix::runtime::KernelSigaction {
                sa_handler_kernel: rustix::runtime::kernel_sig_ign(),
                sa_flags: rustix::runtime::KernelSigactionFlags::empty(),
                sa_restorer: None,
                sa_mask: rustix::runtime::KernelSigSet::empty(),
            };
            let _ = rustix::runtime::kernel_sigaction(sig, Some(action));
        }
        #[cfg(target_os = "macos")]
        {
            darwin_sigaction(sig.as_raw(), 1, 0);
        }
    }
}

/// Whether any trapped signal arrived since the last [`take_pending`].
pub fn any_pending() -> bool {
    ANY_PENDING.load(Ordering::Acquire)
}

/// Check and clear all pending signal flags. Returns the names of the signals
/// that were pending, in signal-number order.
pub fn take_pending() -> Vec<&'static str> {
    if !ANY_PENDING.swap(false, Ordering::AcqRel) {
        return Vec::new();
    }
    PENDING
        .iter()
        .enumerate()
        .filter(|(_, flag)| flag.swap(false, Ordering::AcqRel))
        .filter_map(|(signum, _)| signal_to_name(signum as i32))
        .collect()
}

/// The lowest pending signal number, without clearing it.
pub(crate) fn first_pending() -> Option<i32> {
    if !any_pending() {
        return None;
    }
    PENDING
        .iter()
        .position(|flag| flag.load(Ordering::Acquire))
        .map(|signum| signum as i32)
}

/// Forget signals that arrived before a fork; they belong to the parent.
pub(crate) fn clear_pending() {
    for flag in &PENDING {
        flag.store(false, Ordering::Relaxed);
    }
    ANY_PENDING.store(false, Ordering::Release);
}

/// Async-signal-safe: atomic stores plus, while `wait` is blocked, one write(2).
extern "C" fn signal_handler(signum: i32) {
    if let Some(flag) = usize::try_from(signum).ok().and_then(|n| PENDING.get(n)) {
        flag.store(true, Ordering::Release);
        ANY_PENDING.store(true, Ordering::Release);
    }
    wake_waiter();
}

/// Write end of the self-pipe that wakes a blocked `wait` builtin, or -1.
static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

fn wake_waiter() {
    let fd = WAKE_FD.load(Ordering::Acquire);
    if fd < 0 {
        return;
    }
    // SAFETY: write(2) is async-signal-safe; errno is saved and restored so the
    // interrupted code never observes the handler's errno.
    unsafe {
        let errno = errno_location();
        let saved = *errno;
        let _ = libc::write(fd, [1u8].as_ptr().cast(), 1);
        *errno = saved;
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn errno_location() -> *mut i32 {
    unsafe { libc::__errno_location() }
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
unsafe fn errno_location() -> *mut i32 {
    unsafe { libc::__error() }
}

#[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
unsafe fn errno_location() -> *mut i32 {
    unsafe { libc::__errno() }
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct DarwinSigaction {
    sa_sigaction: usize,
    sa_mask: u32,
    sa_flags: core::ffi::c_int,
}

#[cfg(target_os = "macos")]
unsafe fn darwin_sigaction(signal: i32, handler: usize, flags: core::ffi::c_int) {
    unsafe extern "C" {
        fn sigaction(
            signal: core::ffi::c_int,
            action: *const DarwinSigaction,
            old_action: *mut DarwinSigaction,
        ) -> core::ffi::c_int;
    }
    let action = DarwinSigaction {
        sa_sigaction: handler,
        sa_mask: 0,
        sa_flags: flags,
    };
    let _ = unsafe { sigaction(signal, &action, std::ptr::null_mut()) };
}

/// Reset shell job-control dispositions/masks before executing an interactive child.
/// Uses only async-signal-safe operations, including in a Command pre_exec hook.
pub(crate) fn reset_job_signals() -> std::io::Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        let mut unblock: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut unblock);
        for sig in [
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTSTP,
            libc::SIGTTIN,
            libc::SIGTTOU,
            libc::SIGPIPE,
        ] {
            if libc::sigaction(sig, &action, std::ptr::null_mut()) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            libc::sigaddset(&mut unblock, sig);
        }
        if libc::sigprocmask(libc::SIG_UNBLOCK, &unblock, std::ptr::null_mut()) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

extern "C" fn child_wake_handler(_signal: i32) {
    wake_waiter();
}

/// Wake-up source for the `wait` builtin while it blocks on a background job.
///
/// Self-pipe trick: the trap handler and a temporary `SIGCHLD` handler write a
/// byte to the pipe, and [`WaitWakeup::block`] polls its read end. A signal or a
/// child exit that lands after the caller's last check stays in the pipe, so no
/// wake-up is lost. The previous `SIGCHLD` action is restored on drop.
pub(crate) struct WaitWakeup {
    read: i32,
    write: i32,
    old_child_action: libc::sigaction,
}

impl WaitWakeup {
    pub(crate) fn install() -> std::io::Result<Self> {
        let mut fds = [0; 2];
        // SAFETY: plain syscalls on descriptors this function owns; the sigaction
        // structs are zero-initialized before use and the handler only does
        // async-signal-safe work.
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in fds {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            WAKE_FD.store(fds[1], Ordering::Release);
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut old) == -1 {
                let error = std::io::Error::last_os_error();
                WAKE_FD.store(-1, Ordering::Release);
                libc::close(fds[0]);
                libc::close(fds[1]);
                return Err(error);
            }
            // A `trap ... CHLD` keeps flagging its trap while we wait.
            let handler = if old.sa_sigaction == signal_handler as *const () as usize {
                signal_handler as *const () as usize
            } else {
                child_wake_handler as *const () as usize
            };
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler;
            action.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut());
            Ok(Self {
                read: fds[0],
                write: fds[1],
                old_child_action: old,
            })
        }
    }

    /// Block until a signal or child exit wrote to the pipe, then drain it.
    /// A 100 ms ceiling keeps `wait` live even if `SIGCHLD` is blocked.
    pub(crate) fn block(&self) {
        let mut pollfd = libc::pollfd {
            fd: self.read,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pollfd` and the buffer are valid for the duration of the calls.
        unsafe {
            libc::poll(&mut pollfd, 1, 100);
            let mut buffer = [0u8; 64];
            while libc::read(self.read, buffer.as_mut_ptr().cast(), buffer.len()) > 0 {}
        }
    }
}

impl Drop for WaitWakeup {
    fn drop(&mut self) {
        // SAFETY: restores the action saved in `install` and closes owned fds.
        unsafe {
            libc::sigaction(libc::SIGCHLD, &self.old_child_action, std::ptr::null_mut());
            WAKE_FD.store(-1, Ordering::Release);
            libc::close(self.read);
            libc::close(self.write);
        }
    }
}

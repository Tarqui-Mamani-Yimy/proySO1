use std::io::Write;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use crate::ast::*;
use crate::error::ShellError;
use crate::eval::{RedirectOpen, RedirectOpenMode, SINK_ROUTE_BYPASS, SINK_ROUTE_CLOSED, Shell};
use crate::shell_bytes::ShellBytes;

use crate::sys;

/// Saved file descriptor for restoration after redirections.
pub(crate) struct SavedFd {
    pub(crate) target_fd: RawFd,
    pub(crate) saved_copy: Option<RawFd>,
    /// The target was not open before the redirection, so restoration closes it.
    pub(crate) was_closed: bool,
    pub(crate) prior_route: Option<u8>,
}

/// Restores command redirections on every exit path, including unwinding from
/// an embedder callback.
pub(crate) struct RedirectionGuard {
    saved: Option<Vec<SavedFd>>,
    stdout_depth: Arc<AtomicUsize>,
    stderr_depth: Arc<AtomicUsize>,
    stdout_route: Arc<AtomicU8>,
    stderr_route: Arc<AtomicU8>,
}

impl RedirectionGuard {
    pub(crate) fn new(
        saved: Vec<SavedFd>,
        stdout_depth: Arc<AtomicUsize>,
        stderr_depth: Arc<AtomicUsize>,
        stdout_route: Arc<AtomicU8>,
        stderr_route: Arc<AtomicU8>,
    ) -> Self {
        Self {
            saved: Some(saved),
            stdout_depth,
            stderr_depth,
            stdout_route,
            stderr_route,
        }
    }

    /// Keep the redirections installed, closing only the saved descriptor copies.
    pub(crate) fn persist(&mut self) {
        if let Some(saved) = self.saved.take() {
            for entry in saved {
                if let Some(copy) = entry.saved_copy {
                    sys::close(copy);
                }
            }
        }
    }

    fn restore(&mut self) {
        let Some(saved) = self.saved.take() else {
            return;
        };
        for entry in saved.iter().rev() {
            decrement_redirect_depth(entry.target_fd, &self.stdout_depth, &self.stderr_depth);
            restore_route(entry, &self.stdout_route, &self.stderr_route);
        }
        restore_saved_fds(saved);
    }
}

fn restore_route(entry: &SavedFd, stdout: &AtomicU8, stderr: &AtomicU8) {
    if let Some(route) = entry.prior_route {
        match entry.target_fd {
            1 => stdout.store(route, Ordering::Relaxed),
            2 => stderr.store(route, Ordering::Relaxed),
            _ => {}
        }
    }
}

fn set_route(fd: RawFd, route: u8, shell: &Shell) {
    match fd {
        1 => shell.stdout_sink_route.store(route, Ordering::Relaxed),
        2 => shell.stderr_sink_route.store(route, Ordering::Relaxed),
        _ => {}
    }
}

impl Drop for RedirectionGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn increment_redirect_depth(fd: RawFd, stdout_depth: &AtomicUsize, stderr_depth: &AtomicUsize) {
    match fd {
        1 => {
            stdout_depth.fetch_add(1, Ordering::Relaxed);
        }
        2 => {
            stderr_depth.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

fn decrement_redirect_depth(fd: RawFd, stdout_depth: &AtomicUsize, stderr_depth: &AtomicUsize) {
    match fd {
        1 => {
            stdout_depth.fetch_sub(1, Ordering::Relaxed);
        }
        2 => {
            stderr_depth.fetch_sub(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

fn restore_saved_fds(saved: Vec<SavedFd>) {
    for s in saved.into_iter().rev() {
        if let Some(copy) = s.saved_copy {
            // SAFETY: copy is a valid fd from fcntl_dupfd_cloexec; target_fd is the original fd being restored.
            sys::dup2(copy, s.target_fd);
            sys::close(copy);
        } else if s.was_closed
            // SAFETY: only queried; a redirection that failed before its
            // dup2 left the descriptor closed, and closing it again would
            // report EBADF.
            && rustix::io::fcntl_getfd(unsafe {
                std::os::fd::BorrowedFd::borrow_raw(s.target_fd)
            })
            .is_ok()
        {
            sys::close(s.target_fd);
        }
    }
}

/// Install an opened descriptor without closing it when open reused the target.
fn install_redirection_fd(file: OwnedFd, target_fd: RawFd) -> crate::error::Result<()> {
    if file.as_raw_fd() == target_fd {
        // No dup2 means no automatic CLOEXEC clearing; the shell owns this fd now.
        rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty())
            .map_err(|e| ShellError::Io(e.into()))?;
        let _ = file.into_raw_fd();
    } else {
        sys::dup2(file.as_raw_fd(), target_fd);
    }
    Ok(())
}

impl Shell {
    /// Apply redirections left to right. Returns saved FD state for
    /// restoration. When a later redirection fails, the descriptors changed by
    /// earlier ones are restored before the error is returned.
    pub(crate) fn setup_redirections(
        &mut self,
        redirs: &[Redir],
    ) -> crate::error::Result<Vec<SavedFd>> {
        let mut saved = Vec::new();
        match self.apply_redirections(redirs, &mut saved) {
            Ok(()) => Ok(saved),
            Err(e) => {
                self.restore_redirections(saved);
                Err(e)
            }
        }
    }

    /// Open a writable redirection target, letting an installed provider own
    /// the open. The default open matches `File::create`, append, and `<>`.
    fn open_writable_redirection(
        &mut self,
        path: &std::path::Path,
        mode: RedirectOpenMode,
        target_fd: RawFd,
    ) -> std::io::Result<std::os::fd::OwnedFd> {
        if let Some(handler) = self.redirect_open_handler.as_mut() {
            let request = RedirectOpen {
                path,
                mode,
                fd: target_fd,
            };
            if let Some(result) = handler(&request) {
                return result;
            }
        }
        let mut options = std::fs::OpenOptions::new();
        match mode {
            RedirectOpenMode::Truncate => options.write(true).create(true).truncate(true),
            RedirectOpenMode::Append => options.append(true).create(true),
            RedirectOpenMode::ReadWrite => options.read(true).write(true).create(true),
        };
        options.open(path).map(Into::into)
    }

    fn apply_redirections(
        &mut self,
        redirs: &[Redir],
        saved: &mut Vec<SavedFd>,
    ) -> crate::error::Result<()> {
        for redir in redirs {
            let target_fd = redir.fd;
            let preserve_fd = !(target_fd == 0
                && matches!(
                    redir.kind,
                    RedirKind::HereDoc(_) | RedirKind::HereDocStrip(_)
                )
                && (self.stdout_sink.is_some() || self.stderr_sink.is_some()));

            if preserve_fd {
                // SAFETY: target_fd is only borrowed for the duplicate; an
                // unopened descriptor reports EBADF.
                let target = unsafe { std::os::fd::BorrowedFd::borrow_raw(target_fd) };
                let (saved_copy, was_closed) = match rustix::io::fcntl_dupfd_cloexec(target, 10) {
                    Ok(copy) => (Some(std::os::fd::IntoRawFd::into_raw_fd(copy)), false),
                    Err(e) => (None, e == rustix::io::Errno::BADF),
                };

                saved.push(SavedFd {
                    target_fd,
                    saved_copy,
                    was_closed,
                    prior_route: match target_fd {
                        1 => Some(self.stdout_sink_route.load(Ordering::Relaxed)),
                        2 => Some(self.stderr_sink_route.load(Ordering::Relaxed)),
                        _ => None,
                    },
                });
                increment_redirect_depth(
                    target_fd,
                    &self.stdout_redirect_depth,
                    &self.stderr_redirect_depth,
                );
            }

            if target_fd == 1 || target_fd == 2 {
                let route =
                    if matches!(redir.kind, RedirKind::DupInput(_) | RedirKind::DupOutput(_)) {
                        None
                    } else {
                        Some(SINK_ROUTE_BYPASS)
                    };
                if let Some(route) = route {
                    set_route(target_fd, route, self);
                }
            }

            match &redir.kind {
                RedirKind::Input(word) => {
                    let filename = self.expand_string(word)?;
                    let filepath =
                        self.resolve_path_bytes(&ShellBytes::from_str_lossless(&filename));
                    let file = std::fs::File::open(&filepath).map_err(|e| {
                        self.err_msg(&format!("{filename}: {e}"));
                        ShellError::Io(e)
                    })?;
                    install_redirection_fd(file.into(), target_fd)?;
                }
                RedirKind::Output(word)
                | RedirKind::Clobber(word)
                | RedirKind::Append(word)
                | RedirKind::ReadWrite(word) => {
                    let mode = match redir.kind {
                        RedirKind::Append(_) => RedirectOpenMode::Append,
                        RedirKind::ReadWrite(_) => RedirectOpenMode::ReadWrite,
                        _ => RedirectOpenMode::Truncate,
                    };
                    let filename = self.expand_string(word)?;
                    let filepath =
                        self.resolve_path_bytes(&ShellBytes::from_str_lossless(&filename));
                    let file = self
                        .open_writable_redirection(&filepath, mode, target_fd)
                        .map_err(|e| {
                            self.err_msg(&format!("{filename}: {e}"));
                            ShellError::Io(e)
                        })?;
                    install_redirection_fd(file, target_fd)?;
                }
                RedirKind::DupInput(word) | RedirKind::DupOutput(word) => {
                    let fd_str = self.expand_string(word)?;
                    if fd_str == "-" {
                        set_route(target_fd, SINK_ROUTE_CLOSED, self);
                        // SAFETY: target_fd is a valid fd number from the redirect syntax.
                        sys::close(target_fd);
                    } else if let Ok(source_fd) = fd_str.parse::<i32>() {
                        let route = match source_fd {
                            1 => self.stdout_sink_route.load(Ordering::Relaxed),
                            2 => self.stderr_sink_route.load(Ordering::Relaxed),
                            _ => SINK_ROUTE_BYPASS,
                        };
                        set_route(target_fd, route, self);
                        // SAFETY: source_fd is user-specified (may fail at OS level); target_fd is from redirect syntax.
                        sys::dup2_checked(source_fd, target_fd)
                            .map_err(|e| ShellError::Io(e.into()))?;
                    } else {
                        return Err(ShellError::Runtime {
                            msg: format!("{fd_str}: bad file descriptor"),
                            span: redir.span,
                        });
                    }
                }
                RedirKind::HereDoc(body) | RedirKind::HereDocStrip(body) => {
                    let mut fds = [0i32; 2];
                    // SAFETY: fds is a valid 2-element array for pipe() to write into.
                    sys::pipe(fds.as_mut_ptr());
                    // SAFETY: fds[1] is a valid fd just returned by pipe().
                    let write_end = unsafe { std::fs::File::from_raw_fd(fds[1]) };
                    let read_fd = fds[0];

                    let expanded = match body {
                        HereDocBody::Literal(s) => s.clone(),
                        HereDocBody::Parsed(parts) => {
                            let word = Word {
                                parts: parts.clone(),
                                span: redir.span,
                            };
                            crate::shell_bytes::ShellBytes::from_str_lossless(
                                &self.expand_string(&word)?,
                            )
                        }
                    };
                    let _ = (&write_end).write_all(expanded.as_bytes());
                    drop(write_end);

                    // When sinks are active the shell is embedded in a multi-threaded
                    // process. Calling dup2(read_fd, 0) in the parent would replace the
                    // process-wide fd 0 and race with other threads reading stdin (e.g.
                    // nerv's keyboard input loop). Instead, stash the read fd and let
                    // eval_external pass it via cmd.stdin() after fork — only the child
                    // inherits it. For builtins/functions that need stdin, the dup2 path
                    // is still used since they run in-process on this thread.
                    if target_fd == 0 && (self.stdout_sink.is_some() || self.stderr_sink.is_some())
                    {
                        // Close any previously pending stdin (shouldn't happen in practice).
                        if let Some(old) = self.pending_stdin.take() {
                            // SAFETY: old is a valid fd we own.
                            sys::close(old);
                        }
                        self.pending_stdin = Some(read_fd);
                        // No SavedFd entry needed: we never touched fd 0.
                    } else {
                        // SAFETY: read_fd is valid from pipe(); target_fd is the redirect target.
                        sys::dup2(read_fd, target_fd);
                        sys::close(read_fd);
                    }
                }
            }
        }

        Ok(())
    }

    /// Restore file descriptors after redirections.
    pub(crate) fn restore_redirections(&self, saved: Vec<SavedFd>) {
        for entry in saved.iter().rev() {
            decrement_redirect_depth(
                entry.target_fd,
                &self.stdout_redirect_depth,
                &self.stderr_redirect_depth,
            );
            restore_route(entry, &self.stdout_sink_route, &self.stderr_sink_route);
        }
        restore_saved_fds(saved);
    }
}

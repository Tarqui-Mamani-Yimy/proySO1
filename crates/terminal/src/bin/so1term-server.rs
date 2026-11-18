//! `so1term-server`: persistent headless terminal session server (Phase 5 T10).
//!
//! The server owns the shell PTYs and terminal models of one named session and
//! serves the so1term control protocol on a private Unix socket at
//! `<runtime dir>/so1term-server-<session>.sock`. Clients (`so1ctl` now, the
//! so1term window in T11) can connect and disconnect freely; the shells keep
//! running. The server exits when the last pane closes or on `so1ctl shutdown`,
//! removing its socket. It ignores SIGHUP so closing the launching terminal does
//! not end the session.

use std::process::ExitCode;

const USAGE: &str = "Usage: so1term-server [--session NAME]\n\
Runs a persistent so1term session (default name: default) that outlives its clients.\n\
Control it with so1ctl; stop it with `so1ctl shutdown` or by exiting its last shell.";

fn main() -> ExitCode {
    #[cfg(unix)]
    {
        unix::run()
    }
    #[cfg(not(unix))]
    {
        eprintln!("so1term-server: persistent sessions are not supported on this platform yet");
        ExitCode::from(1)
    }
}

/// Parses the command line; `Ok(None)` means help was printed.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<String>, String> {
    let mut args = args.into_iter();
    let mut session = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--session" if session.is_none() => {
                session = Some(args.next().ok_or("--session requires a name")?);
            }
            "--session" => return Err("--session may only be supplied once".into()),
            other => return Err(format!("unexpected argument: {other}\n{USAGE}")),
        }
    }
    let session = session.unwrap_or_else(|| "default".to_owned());
    so1_terminal::control::validate_session_name(&session)?;
    Ok(Some(session))
}

#[cfg(unix)]
mod unix {
    use so1_terminal::{
        control::{
            self, ControlError, ControlErrorCode, ControlOutcome, ControlRequest, ControlResponse,
            ControlServer,
        },
        layout::PaneId,
        session::{ControlEffect, DEFAULT_COLS, DEFAULT_ROWS, OutputNotifier, Session},
    };
    use std::{
        io,
        path::PathBuf,
        process::ExitCode,
        sync::{Arc, mpsc},
        time::Duration,
    };

    /// Matches the window's control request timeout.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
    /// Fallback interval for noticing exited shells without new output.
    const REAP_INTERVAL: Duration = Duration::from_millis(250);
    /// Upper bound for letting clients read their final reply before exit.
    const CLIENT_DRAIN: Duration = Duration::from_secs(2);

    enum Event {
        Output(PaneId),
        Control(ControlRequest, mpsc::SyncSender<ControlResponse>),
    }

    fn unavailable(id: u64) -> ControlResponse {
        ControlResponse {
            id: Some(id),
            outcome: ControlOutcome::Failure(ControlError::for_request(
                Some(id),
                ControlErrorCode::Unavailable,
                "session server is not answering",
            )),
        }
    }

    pub fn run() -> ExitCode {
        let session_name = match super::parse_args(std::env::args().skip(1)) {
            Ok(Some(name)) => name,
            Ok(None) => return ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("so1term-server: {message}");
                return ExitCode::from(2);
            }
        };
        // The session must survive the terminal that launched it.
        unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };

        let (events, receiver) = mpsc::channel::<Event>();
        let control_events = events.clone();
        let started = control::runtime_directory().and_then(|directory| {
            ControlServer::start_session_at(&directory, &session_name, move |request| {
                let id = request.id;
                let (reply, response) = mpsc::sync_channel(1);
                if control_events.send(Event::Control(request, reply)).is_err() {
                    return unavailable(id);
                }
                response
                    .recv_timeout(REQUEST_TIMEOUT)
                    .unwrap_or_else(|_| unavailable(id))
            })
        });
        let server = match started {
            Ok(server) => server,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                eprintln!("so1term-server: session {session_name} is already running");
                return ExitCode::from(1);
            }
            Err(error) => {
                eprintln!("so1term-server: cannot start the control socket: {error}");
                return ExitCode::from(1);
            }
        };
        // Exported before the first shell starts so every pane inherits it.
        unsafe { std::env::set_var("SO1TERM_SOCKET", server.socket_path()) };

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let notify: OutputNotifier = Arc::new(move |pane| {
            let _ = events.send(Event::Output(pane));
        });
        let mut session = match Session::start(DEFAULT_COLS, DEFAULT_ROWS, cwd, notify) {
            Ok(session) => session,
            Err(error) => {
                eprintln!("so1term-server: {error}");
                return ExitCode::from(1);
            }
        };
        println!(
            "so1term-server: session {session_name} listening on {}",
            server.socket_path().display()
        );

        loop {
            match receiver.recv_timeout(REAP_INTERVAL) {
                Ok(Event::Output(pane)) => session.receive_output(pane),
                Ok(Event::Control(request, reply)) => {
                    let (response, effect) = session.handle_control(request);
                    let _ = reply.send(response);
                    if effect == ControlEffect::Shutdown {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            session.reap_exited();
            if session.is_empty() {
                break;
            }
        }
        // Requests queued behind the final one fail fast instead of waiting for the drain.
        while let Ok(event) = receiver.try_recv() {
            if let Event::Control(request, reply) = event {
                let _ = reply.send(unavailable(request.id));
            }
        }
        drop(session);
        server.wait_for_clients(CLIENT_DRAIN);
        drop(server);
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn session_defaults_to_default_and_accepts_one_name() {
        assert_eq!(parse_args(args(&[])), Ok(Some("default".into())));
        assert_eq!(
            parse_args(args(&["--session", "work"])),
            Ok(Some("work".into()))
        );
        assert!(parse_args(args(&["--session"])).is_err());
        assert!(parse_args(args(&["--session", "a", "--session", "b"])).is_err());
        assert!(parse_args(args(&["--bogus"])).is_err());
        assert!(
            parse_args(args(&["--session", "../x"]))
                .unwrap_err()
                .contains("invalid session name")
        );
    }
}

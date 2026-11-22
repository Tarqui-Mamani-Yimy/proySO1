//! End-to-end test of the GUI client transport (Phase 5 T11c) against the real
//! `nulix-server`: `ensure_server` starts the session in an isolated runtime
//! directory, the remote.rs attach connection receives Welcome, Layout and a full
//! Frame, typed input shows up in a later frame, and detaching leaves the
//! session running.
#![cfg(unix)]

use nulix::{
    attach::{ClientMessage, ServerMessage},
    control::{ControlCommand, ControlOutcome, ControlRequest, send_request},
    remote::{
        AttachConnection, FrameOutcome, ImageChange, RemoteEvent, RemoteView, ViewChange,
        WindowMetrics, ensure_server,
    },
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

const SERVER: &str = env!("CARGO_BIN_EXE_nulix-server");
const WAIT: Duration = Duration::from_secs(10);
const SESSION: &str = "remote-client";

/// Isolated runtime directory; shuts the detached server down and removes the
/// directory on drop, even when an assertion fails.
struct Session {
    root: PathBuf,
    socket: Option<PathBuf>,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(socket) = &self.socket {
            let _ = send_request(
                socket,
                &ControlRequest {
                    id: 99,
                    command: ControlCommand::Shutdown,
                },
            );
            let deadline = Instant::now() + WAIT;
            while socket.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn next_event(events: &Receiver<RemoteEvent>) -> RemoteEvent {
    events
        .recv_timeout(WAIT)
        .expect("attach stream event within the timeout")
}

fn row_texts(view: &RemoteView, pane: u64) -> Vec<String> {
    view.pane(pane).map_or_else(Vec::new, |pane| {
        pane.cells
            .iter()
            .map(|row| row.iter().map(|cell| cell.character).collect())
            .collect()
    })
}

fn list_succeeds(socket: &Path) -> bool {
    send_request(
        socket,
        &ControlRequest {
            id: 7,
            command: ControlCommand::List,
        },
    )
    .is_ok_and(|response| matches!(response.outcome, ControlOutcome::Success(_)))
}

#[test]
fn ensure_server_attach_input_and_detach_keep_the_session_running() {
    let root = std::env::temp_dir().join(format!("nulix-remote-client-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let mut session = Session {
        root: root.clone(),
        socket: None,
    };
    // The test binary lives in target/<profile>/deps, so the sibling lookup misses
    // and ensure_server falls back to PATH, which names the freshly built server.
    let server_dir = Path::new(SERVER).parent().unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![server_dir.to_path_buf()];
    dirs.extend(std::env::split_paths(&path));
    // SAFETY: this integration-test binary has a single test, so no other thread reads the environment.
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &root);
        std::env::set_var("NULIX_SHELL", "/bin/sh");
        std::env::set_var("PATH", std::env::join_paths(dirs).unwrap());
        std::env::remove_var("NULIX_SOCKET");
    }

    let socket = ensure_server(SESSION).expect("ensure_server starts the session server");
    session.socket = Some(socket.clone());
    assert_eq!(
        socket,
        root.join("nulix")
            .join(format!("nulix-server-{SESSION}.sock"))
    );
    // A second call finds the live server instead of starting another one.
    assert_eq!(ensure_server(SESSION).unwrap(), socket);

    let (sender, events) = mpsc::channel();
    let connection = AttachConnection::attach(
        &socket,
        WindowMetrics::new(800, 480, 8.0, 16.0),
        move |event| {
            let _ = sender.send(event);
        },
    )
    .expect("attach to the running session");
    assert_eq!(connection.session(), SESSION);

    assert_eq!(
        next_event(&events),
        RemoteEvent::Message(ServerMessage::Welcome {
            session: SESSION.into()
        })
    );
    let mut view = RemoteView::default();
    let RemoteEvent::Message(layout @ ServerMessage::Layout(_)) = next_event(&events) else {
        panic!("Layout follows Welcome");
    };
    view.apply(layout);
    let focused = view.focused_pane().expect("layout has a focused pane").0;
    let RemoteEvent::Message(ServerMessage::Frame(frame)) = next_event(&events) else {
        panic!("a full Frame follows Layout");
    };
    assert!(frame.full, "first frame after attach is full");
    assert_eq!(frame.pane, focused);
    let first_seq = frame.seq;
    assert_eq!(
        view.apply_frame(frame),
        FrameOutcome::Applied,
        "full frame applies"
    );
    let pane = view.pane(focused).unwrap();
    assert_eq!((pane.cols, pane.rows), (100, 30));

    connection
        .send(&ClientMessage::Input {
            pane: focused,
            data: b"echo remote-$((40+2))\n".to_vec(),
        })
        .unwrap();
    let deadline = Instant::now() + WAIT;
    let mut later_seq = first_seq;
    while !row_texts(&view, focused)
        .iter()
        .any(|row| row.trim_end() == "remote-42")
    {
        assert!(
            Instant::now() < deadline,
            "typed command output never appeared: {:?}",
            row_texts(&view, focused)
        );
        let Ok(event) = events.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        match event {
            RemoteEvent::Message(message) => {
                if let ServerMessage::Frame(frame) = &message
                    && frame.pane == focused
                {
                    later_seq = frame.seq;
                }
                if let ViewChange::Frame { outcome, .. } = view.apply(message) {
                    assert_ne!(outcome, FrameOutcome::ResyncNeeded, "no sequence gap");
                }
            }
            RemoteEvent::Closed(reason) => panic!("stream closed early: {reason:?}"),
        }
    }
    assert!(later_seq > first_seq, "output arrived in a later frame");

    // A kitty image printed in the pane reaches the view as complete RGBA data
    // that the pane's placements reference.
    connection
        .send(&ClientMessage::Input {
            pane: focused,
            data: b"printf '\\033_Ga=T,f=32,s=1,v=1,i=6,q=2;/wAA/w==\\033\\\\'\n".to_vec(),
        })
        .unwrap();
    let deadline = Instant::now() + WAIT;
    let mut ready = false;
    while !(ready
        && view
            .pane(focused)
            .is_some_and(|pane| pane.placements.iter().any(|p| p.image == 6)))
    {
        assert!(Instant::now() < deadline, "image never reached the view");
        let Ok(event) = events.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        let RemoteEvent::Message(message) = event else {
            panic!("stream closed early: {event:?}");
        };
        if view.apply(message)
            == (ViewChange::Image {
                pane: focused,
                image: 6,
                change: ImageChange::Ready,
            })
        {
            ready = true;
        }
    }
    assert_eq!(view.image(focused, 6).unwrap().rgba, vec![255, 0, 0, 255]);

    connection.detach().unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(
            Instant::now() < deadline,
            "server did not close the detached stream"
        );
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(RemoteEvent::Closed(None)) => break,
            Ok(RemoteEvent::Closed(Some(reason))) => panic!("detach broke the stream: {reason}"),
            _ => {}
        }
    }
    assert!(list_succeeds(&socket), "session still answers after detach");
    assert!(socket.exists());
}

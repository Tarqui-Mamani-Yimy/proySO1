//! End-to-end tests for the `nulix-server` attach stream (Phase 5 T11b/T11d).
//!
//! Each test runs the real server binary with an isolated `XDG_RUNTIME_DIR`,
//! upgrades a control connection with the `attach` request, and drives the
//! session with a raw attach-protocol client built on the shared codec.
#![cfg(unix)]

use alacritty_terminal::selection::SelectionType;
use nulix::{
    attach::{
        BlockOp, ClientMessage, CopySource, FramePlacement, LayoutAction, LayoutFrame, PaneFrame,
        SelectionOp, ServerMessage, SplitDirection, cells_from_runs, encode_client, parse_server,
    },
    control::{
        ControlCommand, ControlErrorCode, ControlOutcome, ControlRequest, ControlResponse,
        parse_response, serialize_request,
    },
};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, ErrorKind, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

const SERVER: &str = env!("CARGO_BIN_EXE_nulix-server");
const CTL: &str = env!("CARGO_BIN_EXE_nulixctl");
const WAIT: Duration = Duration::from_secs(10);
/// Window metrics used by most tests: 100x30 cells of 8x16 px.
const HELLO: ClientMessage = ClientMessage::Hello {
    width: 800,
    height: 480,
    cell_width: 8,
    cell_height: 16,
};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

/// Scratch runtime directory outside the worktree, removed on drop.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "nulix-attach-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }

    fn socket(&self, session: &str) -> PathBuf {
        self.root
            .join("nulix")
            .join(format!("nulix-server-{session}.sock"))
    }

    fn spawn(&self, session: &str) -> Server {
        let child = Command::new(SERVER)
            .args(["--session", session])
            .env("XDG_RUNTIME_DIR", &self.root)
            .env("NULIX_SHELL", "/bin/sh")
            // Never read the developer's own nulix.conf.
            .env("NULIX_CONFIG", "/dev/null")
            .env("PS1", "$ ")
            .env_remove("NULIX_SOCKET")
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let server = Server { child };
        wait_until("server socket", || {
            UnixStream::connect(self.socket(session)).is_ok()
        });
        server
    }

    fn ctl(&self, session: &str, args: &[&str]) -> Output {
        Command::new(CTL)
            .arg("--socket")
            .arg(self.socket(session))
            .args(args)
            .env_remove("NULIX_SOCKET")
            .output()
            .unwrap()
    }

    /// Sends the control `attach` request and returns the connection and its reply.
    fn attach(&self, session: &str) -> (Client, ControlResponse) {
        let stream = UnixStream::connect(self.socket(session)).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            views: BTreeMap::new(),
            layout: None,
            images: BTreeMap::new(),
            announcements: Vec::new(),
            removed: Vec::new(),
        };
        let request = serialize_request(&ControlRequest {
            id: 7,
            command: ControlCommand::Attach,
        })
        .unwrap();
        client.stream.write_all(&request).unwrap();
        let mut line = Vec::new();
        client.reader.read_until(b'\n', &mut line).unwrap();
        (client, parse_response(&line).unwrap())
    }

    /// Attaches, sends `HELLO`, and checks the handshake order; returns the client.
    fn attach_hello(&self, session: &str) -> Client {
        let (mut client, reply) = self.attach(session);
        assert_attached(&reply, session);
        client.send(&HELLO);
        assert_eq!(
            client.recv(),
            ServerMessage::Welcome {
                session: session.into()
            }
        );
        let ServerMessage::Layout(layout) = client.recv() else {
            panic!("expected layout after welcome")
        };
        let visible = visible_panes(&layout);
        for _ in &visible {
            // Image data the frames reference precedes them.
            let frame = client.recv_until("handshake frame", |message| match message {
                ServerMessage::Frame(frame) => Some(frame.clone()),
                ServerMessage::Image { .. } | ServerMessage::ImageData { .. } => None,
                other => panic!("unexpected handshake message {other:?}"),
            });
            assert!(frame.full, "handshake frames are full");
            assert!(visible.contains(&frame.pane));
        }
        client
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Server {
    child: Child,
}

impl Server {
    fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "server did not exit in time");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.is_running() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Client-side view of one pane, maintained with the PaneFrame sequence rule.
struct PaneView {
    seq: u64,
    rows: Vec<String>,
    placements: Vec<FramePlacement>,
}

/// Image data received for one (pane, image): announced size and bytes so far.
struct ReceivedImage {
    width: u32,
    height: u32,
    bytes: u64,
    data: Vec<u8>,
}

/// Raw attach-protocol test client.
struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    views: BTreeMap<u64, PaneView>,
    layout: Option<LayoutFrame>,
    images: BTreeMap<(u64, u32), ReceivedImage>,
    /// Every `Image` announcement, in arrival order.
    announcements: Vec<(u64, u32)>,
    /// Every `ImageRemoved`, in arrival order.
    removed: Vec<(u64, u32)>,
}

impl Client {
    fn send(&mut self, message: &ClientMessage) {
        self.stream
            .write_all(&encode_client(message).unwrap())
            .unwrap();
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
    }

    /// Reads one server message and applies frames/layouts to the local view.
    fn recv(&mut self) -> ServerMessage {
        let mut line = Vec::new();
        let read = self.reader.read_until(b'\n', &mut line).unwrap();
        assert!(read > 0, "attach stream closed unexpectedly");
        let message = parse_server(&line).unwrap();
        match &message {
            ServerMessage::Frame(frame) => self.apply(frame),
            ServerMessage::Layout(layout) => {
                if let Some(previous) = &self.layout {
                    assert!(layout.revision > previous.revision, "revision must grow");
                }
                self.layout = Some(layout.clone());
            }
            ServerMessage::Image {
                pane,
                image,
                width,
                height,
                bytes,
            } => {
                self.announcements.push((*pane, *image));
                self.images.insert(
                    (*pane, *image),
                    ReceivedImage {
                        width: *width,
                        height: *height,
                        bytes: *bytes,
                        data: Vec::new(),
                    },
                );
            }
            ServerMessage::ImageData {
                pane,
                image,
                offset,
                data,
            } => {
                let received = self
                    .images
                    .get_mut(&(*pane, *image))
                    .expect("image data after its announcement");
                assert_eq!(*offset, received.data.len() as u64, "chunks are contiguous");
                received.data.extend_from_slice(data);
                assert!(received.data.len() as u64 <= received.bytes);
            }
            ServerMessage::ImageRemoved { pane, image } => {
                self.removed.push((*pane, *image));
                self.images.remove(&(*pane, *image));
            }
            _ => {}
        }
        message
    }

    /// Complete RGBA data of an image, once every chunk arrived.
    fn image(&self, pane: u64, image: u32) -> Option<(u32, u32, &[u8])> {
        let received = self.images.get(&(pane, image))?;
        (received.data.len() as u64 == received.bytes).then_some((
            received.width,
            received.height,
            received.data.as_slice(),
        ))
    }

    fn placements(&self, pane: u64) -> Vec<FramePlacement> {
        self.views
            .get(&pane)
            .map(|view| view.placements.clone())
            .unwrap_or_default()
    }

    fn apply(&mut self, frame: &PaneFrame) {
        for placement in &frame.placements {
            assert!(
                self.images.contains_key(&(frame.pane, placement.image)),
                "image {} is announced before a frame references it",
                placement.image
            );
        }
        let rows: Vec<String> = frame
            .lines
            .iter()
            .map(|line| {
                cells_from_runs(&line.runs)
                    .iter()
                    .map(|cell| cell.character)
                    .collect()
            })
            .collect();
        if frame.full {
            let mut view = vec![String::new(); frame.rows];
            for (line, text) in frame.lines.iter().zip(rows) {
                view[line.row] = text;
            }
            self.views.insert(
                frame.pane,
                PaneView {
                    seq: frame.seq,
                    rows: view,
                    placements: frame.placements.clone(),
                },
            );
        } else {
            let view = self
                .views
                .get_mut(&frame.pane)
                .expect("delta frame before a full frame");
            assert_eq!(frame.seq, view.seq + 1, "delta seq must be last+1");
            view.seq = frame.seq;
            view.placements = frame.placements.clone();
            for (line, text) in frame.lines.iter().zip(rows) {
                view.rows[line.row] = text;
            }
        }
    }

    fn expect_frame(&mut self) -> PaneFrame {
        match self.recv() {
            ServerMessage::Frame(frame) => frame,
            other => panic!("expected frame, got {other:?}"),
        }
    }

    /// Reads messages until `matches` returns a value.
    fn recv_until<T>(
        &mut self,
        what: &str,
        mut matches: impl FnMut(&ServerMessage) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            let message = self.recv();
            if let Some(found) = matches(&message) {
                return found;
            }
        }
    }

    /// Waits until the pane's view contains `needle`; returns the frames received meanwhile.
    fn wait_text(&mut self, pane: u64, needle: &str) -> Vec<PaneFrame> {
        let mut frames = Vec::new();
        while !self.text(pane).contains(needle) {
            let frame = self.recv_until(&format!("text {needle:?}"), |message| match message {
                ServerMessage::Frame(frame) if frame.pane == pane => Some(frame.clone()),
                _ => None,
            });
            frames.push(frame);
        }
        frames
    }

    fn text(&self, pane: u64) -> String {
        self.views
            .get(&pane)
            .map(|view| view.rows.join("\n"))
            .unwrap_or_default()
    }

    fn row_of(&self, pane: u64, trimmed: &str) -> Option<usize> {
        self.views
            .get(&pane)?
            .rows
            .iter()
            .position(|row| row.trim_end() == trimmed)
    }

    fn focused_pane(&self) -> u64 {
        let layout = self.layout.as_ref().expect("layout received");
        layout.tabs[layout.active_tab]
            .panes
            .iter()
            .find(|pane| pane.focused)
            .expect("focused pane")
            .pane
    }

    /// True once the server closed the stream.
    fn closed(&mut self) -> bool {
        let mut line = Vec::new();
        loop {
            line.clear();
            match self.reader.read_until(b'\n', &mut line) {
                Ok(0) => return true,
                Ok(_) => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return false,
                Err(error) if error.kind() == ErrorKind::TimedOut => return false,
                Err(_) => return true,
            }
        }
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn assert_attached(reply: &ControlResponse, session: &str) {
    assert_eq!(reply.id, Some(7));
    assert_eq!(
        reply.outcome,
        ControlOutcome::Success(serde_json::json!({"attached": true, "session": session}))
    );
}

fn visible_panes(layout: &LayoutFrame) -> Vec<u64> {
    layout.tabs[layout.active_tab]
        .panes
        .iter()
        .filter(|pane| !pane.hidden)
        .map(|pane| pane.pane)
        .collect()
}

fn ctl_json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "nulixctl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn input(client: &mut Client, pane: u64, text: &str) {
    client.send(&ClientMessage::Input {
        pane,
        data: text.as_bytes().to_vec(),
    });
}

#[test]
fn handshake_sends_welcome_layout_and_full_frames_in_order() {
    let scratch = Scratch::new();
    let mut server = scratch.spawn("hs");
    let (mut client, reply) = scratch.attach("hs");
    assert_attached(&reply, "hs");
    client.send(&HELLO);
    assert_eq!(
        client.recv(),
        ServerMessage::Welcome {
            session: "hs".into()
        }
    );
    let ServerMessage::Layout(layout) = client.recv() else {
        panic!("expected layout")
    };
    assert_eq!(layout.active_tab, 0);
    assert_eq!(layout.tabs.len(), 1);
    let pane = layout.tabs[0].panes[0];
    assert_eq!(layout.tabs[0].panes.len(), 1);
    assert_eq!((pane.x, pane.y, pane.width, pane.height), (0, 0, 800, 480));
    assert_eq!((pane.cols, pane.rows), (100, 30));
    assert!(pane.focused && !pane.hidden);
    let frame = client.expect_frame();
    assert_eq!(frame.pane, pane.pane);
    assert!(frame.full);
    assert_eq!(frame.seq, 1);
    assert_eq!((frame.cols, frame.rows), (100, 30));
    assert_eq!(frame.lines.len(), 30, "a full frame lists every row");
    assert!(
        frame
            .lines
            .iter()
            .all(|line| { cells_from_runs(&line.runs).len() == 100 })
    );
    assert!(server.is_running());
}

#[test]
fn typed_input_produces_a_delta_frame_with_the_next_seq() {
    let scratch = Scratch::new();
    let _server = scratch.spawn("input");
    let mut client = scratch.attach_hello("input");
    let pane = client.focused_pane();
    input(&mut client, pane, "echo attach-$((40+2))\n");
    let frames = client.wait_text(pane, "attach-42");
    let last = frames.last().unwrap();
    assert!(!last.full, "output arrives as a delta");
    // Client::apply asserted every delta seq was last+1; deltas list only changed rows.
    assert!(last.lines.len() < last.rows);
    assert!(last.lines.iter().any(|line| {
        cells_from_runs(&line.runs)
            .iter()
            .map(|cell| cell.character)
            .collect::<String>()
            .contains("attach-42")
    }));
}

#[test]
fn a_second_attach_is_refused_while_a_client_is_attached() {
    let scratch = Scratch::new();
    let mut server = scratch.spawn("single");
    let mut first = scratch.attach_hello("single");
    let (mut second, reply) = scratch.attach("single");
    let ControlOutcome::Failure(error) = &reply.outcome else {
        panic!("second attach must fail: {reply:?}")
    };
    assert_eq!(error.code, ControlErrorCode::OperationRefused);
    assert_eq!(
        error.message,
        "another client is attached to session single"
    );
    assert!(second.closed(), "refused connection is closed");
    // The first client is unaffected.
    let pane = first.focused_pane();
    first.send(&ClientMessage::Resync { pane: Some(pane) });
    let frame = first.recv_until("resync frame", |message| match message {
        ServerMessage::Frame(frame) if frame.full => Some(frame.clone()),
        _ => None,
    });
    assert_eq!(frame.pane, pane);
    assert!(server.is_running());
}

#[test]
fn detach_keeps_the_shell_and_reattach_gets_full_frames_with_earlier_output() {
    let scratch = Scratch::new();
    let mut server = scratch.spawn("reattach");
    let mut first = scratch.attach_hello("reattach");
    let pane = first.focused_pane();
    input(&mut first, pane, "echo before-$((1+1))-detach\n");
    first.wait_text(pane, "before-2-detach");
    first.send(&ClientMessage::Detach);
    wait_until("detached stream close", || first.closed());
    assert!(server.is_running());

    let mut second = None;
    wait_until("re-attach", || {
        let (client, reply) = scratch.attach("reattach");
        let ok = matches!(reply.outcome, ControlOutcome::Success(_));
        if ok {
            second = Some(client);
        }
        ok
    });
    let mut second = second.unwrap();
    second.send(&HELLO);
    assert!(matches!(second.recv(), ServerMessage::Welcome { .. }));
    assert!(matches!(second.recv(), ServerMessage::Layout(_)));
    let frame = second.expect_frame();
    assert!(frame.full);
    assert_eq!(frame.pane, pane);
    assert!(second.text(pane).contains("before-2-detach"));
    assert!(server.is_running());
}

#[test]
fn resize_changes_grid_and_pty_size_and_sends_full_frames() {
    let scratch = Scratch::new();
    let _server = scratch.spawn("resize");
    let mut client = scratch.attach_hello("resize");
    let pane = client.focused_pane();
    let revision = client.layout.as_ref().unwrap().revision;
    client.send(&ClientMessage::Resize {
        width: 640,
        height: 320,
        cell_width: 8,
        cell_height: 16,
    });
    let layout = client.recv_until("resized layout", |message| match message {
        ServerMessage::Layout(layout) => Some(layout.clone()),
        _ => None,
    });
    assert!(layout.revision > revision);
    let geometry = layout.tabs[0].panes[0];
    assert_eq!((geometry.width, geometry.height), (640, 320));
    assert_eq!((geometry.cols, geometry.rows), (80, 20));
    let frame = client.recv_until("full frame after resize", |message| match message {
        ServerMessage::Frame(frame) if frame.full => Some(frame.clone()),
        _ => None,
    });
    assert_eq!((frame.pane, frame.cols, frame.rows), (pane, 80, 20));
    input(&mut client, pane, "stty size\n");
    client.wait_text(pane, "20 80");
}

#[test]
fn resync_returns_full_frames_for_one_or_all_panes() {
    let scratch = Scratch::new();
    let _server = scratch.spawn("resync");
    let mut client = scratch.attach_hello("resync");
    let pane = client.focused_pane();
    let before = client.views[&pane].seq;
    client.send(&ClientMessage::Resync { pane: Some(pane) });
    let frame = client.recv_until("resync frame", |message| match message {
        ServerMessage::Frame(frame) if frame.full => Some(frame.clone()),
        _ => None,
    });
    assert_eq!(frame.pane, pane);
    assert!(
        frame.seq > before,
        "seq keeps increasing across full frames"
    );
    assert_eq!(frame.lines.len(), frame.rows);
    client.send(&ClientMessage::Resync { pane: None });
    let all = client.recv_until("resync-all frame", |message| match message {
        ServerMessage::Frame(frame) if frame.full => Some(frame.clone()),
        _ => None,
    });
    assert_eq!(all.pane, pane);
}

#[test]
fn split_layout_action_adds_a_pane_with_a_new_revision() {
    let scratch = Scratch::new();
    let _server = scratch.spawn("split");
    let mut client = scratch.attach_hello("split");
    let first = client.focused_pane();
    let revision = client.layout.as_ref().unwrap().revision;
    client.send(&ClientMessage::Layout {
        request: 11,
        action: LayoutAction::Split {
            direction: SplitDirection::Right,
        },
    });
    let mut result = None;
    let mut layout = None;
    while result.is_none() || layout.is_none() {
        match client.recv() {
            ServerMessage::LayoutResult { request, error } => result = Some((request, error)),
            ServerMessage::Layout(frame) => layout = Some(frame),
            _ => {}
        }
    }
    assert_eq!(result, Some((11, None)));
    let layout = layout.unwrap();
    assert!(layout.revision > revision);
    assert_eq!(layout.tabs.len(), 1);
    let panes = &layout.tabs[0].panes;
    assert_eq!(panes.len(), 2);
    assert!(panes.iter().all(|pane| !pane.hidden));
    let second = panes.iter().find(|pane| pane.pane != first).unwrap();
    assert!(second.focused);
    assert!(second.x > 0, "right split sits to the right");
    let frame = client.recv_until("new pane frame", |message| match message {
        ServerMessage::Frame(frame) if frame.pane == second.pane => Some(frame.clone()),
        _ => None,
    });
    assert!(frame.full);
    assert_eq!(frame.cols, second.cols);
}

#[test]
fn layout_actions_manage_tabs_and_reserve_the_tab_bar() {
    let scratch = Scratch::new();
    let _server = scratch.spawn("tabs");
    let mut client = scratch.attach_hello("tabs");
    client.send(&ClientMessage::Layout {
        request: 1,
        action: LayoutAction::NewTab,
    });
    let layout = client.recv_until("two tabs", |message| match message {
        ServerMessage::Layout(layout) if layout.tabs.len() == 2 => Some(layout.clone()),
        _ => None,
    });
    assert_eq!(layout.active_tab, 1);
    for tab in &layout.tabs {
        let pane = tab.panes[0];
        assert_eq!((pane.y, pane.height, pane.rows), (16, 464, 29));
    }
    client.send(&ClientMessage::Layout {
        request: 2,
        action: LayoutAction::SelectTab { index: 5 },
    });
    let error = client.recv_until("select error", |message| match message {
        ServerMessage::LayoutResult { request: 2, error } => Some(error.clone()),
        _ => None,
    });
    assert!(error.is_some(), "out-of-range tab index is reported");
    client.send(&ClientMessage::Layout {
        request: 3,
        action: LayoutAction::CloseTab,
    });
    let layout = client.recv_until("one tab", |message| match message {
        ServerMessage::Layout(layout) if layout.tabs.len() == 1 => Some(layout.clone()),
        _ => None,
    });
    let pane = layout.tabs[0].panes[0];
    assert_eq!((pane.y, pane.rows), (0, 30), "tab bar removed with one tab");
}

#[test]
fn copy_of_a_selection_returns_its_text() {
    let scratch = Scratch::new();
    let _server = scratch.spawn("copy");
    let mut client = scratch.attach_hello("copy");
    let pane = client.focused_pane();
    input(&mut client, pane, "echo copy-$((2+3))-me\n");
    client.wait_text(pane, "\ncopy-5-me");
    let row = client.row_of(pane, "copy-5-me").expect("output row");
    client.send(&ClientMessage::Selection {
        pane,
        op: SelectionOp::Begin {
            column: 0,
            row,
            kind: SelectionType::Simple,
        },
    });
    client.send(&ClientMessage::Selection {
        pane,
        op: SelectionOp::Extend { column: 8, row },
    });
    let frame = client.recv_until("selection frame", |message| match message {
        ServerMessage::Frame(frame) if !frame.selection.is_empty() => Some(frame.clone()),
        _ => None,
    });
    assert_eq!(frame.selection.len(), 1);
    assert_eq!(
        (
            frame.selection[0].row,
            frame.selection[0].start,
            frame.selection[0].end
        ),
        (row, 0, 8)
    );
    client.send(&ClientMessage::Copy {
        request: 21,
        pane,
        source: CopySource::Selection,
    });
    let text = client.recv_until("copy text", |message| match message {
        ServerMessage::CopyText { request: 21, text } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(text.as_deref(), Some("copy-5-me"));

    client.send(&ClientMessage::Selection {
        pane,
        op: SelectionOp::Clear,
    });
    client.send(&ClientMessage::Copy {
        request: 22,
        pane,
        source: CopySource::Selection,
    });
    let text = client.recv_until("empty copy", |message| match message {
        ServerMessage::CopyText { request: 22, text } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(text, None);
    // Block actions run on the server model without breaking the stream.
    client.send(&ClientMessage::Block {
        pane,
        op: BlockOp::Previous,
    });
    client.send(&ClientMessage::Copy {
        request: 23,
        pane,
        source: CopySource::BlockOutput,
    });
    client.recv_until("block copy reply", |message| match message {
        ServerMessage::CopyText { request: 23, .. } => Some(()),
        _ => None,
    });
}

#[test]
fn malformed_lines_and_unknown_panes_get_errors_and_keep_the_stream() {
    let scratch = Scratch::new();
    let _server = scratch.spawn("errors");
    let mut client = scratch.attach_hello("errors");
    let pane = client.focused_pane();
    client.send_raw(b"not json\n");
    let code = client.recv_until("malformed error", |message| match message {
        ServerMessage::Error { code, .. } => Some(code.clone()),
        _ => None,
    });
    assert_eq!(code, "MALFORMED_JSON");
    client.send(&ClientMessage::ScrollToBottom { pane: 9999 });
    let code = client.recv_until("unknown pane error", |message| match message {
        ServerMessage::Error { code, .. } => Some(code.clone()),
        _ => None,
    });
    assert_eq!(code, "NOT_FOUND");
    input(&mut client, pane, "echo still-$((3*3))\n");
    client.wait_text(pane, "still-9");
}

#[test]
fn last_shell_exit_sends_exit_before_the_server_stops() {
    let scratch = Scratch::new();
    let mut server = scratch.spawn("lastexit");
    let mut client = scratch.attach_hello("lastexit");
    let pane = client.focused_pane();
    input(&mut client, pane, "exit\n");
    let reason = client.recv_until("exit message", |message| match message {
        ServerMessage::Exit { reason } => Some(reason.clone()),
        _ => None,
    });
    assert_eq!(reason, "last-shell-exited");
    assert!(server.wait_exit().success());
    assert!(!scratch.socket("lastexit").exists());
}

#[test]
fn nulixctl_works_while_attached_and_shutdown_sends_exit() {
    let scratch = Scratch::new();
    let mut server = scratch.spawn("ctl");
    let mut client = scratch.attach_hello("ctl");
    let list = ctl_json(&scratch.ctl("ctl", &["list"]));
    assert_eq!(list["tabs"].as_array().unwrap().len(), 1);
    ctl_json(&scratch.ctl("ctl", &["new-tab"]));
    client.recv_until("layout after nulixctl new-tab", |message| match message {
        ServerMessage::Layout(layout) if layout.tabs.len() == 2 => Some(()),
        _ => None,
    });
    let reply = ctl_json(&scratch.ctl("ctl", &["shutdown"]));
    assert_eq!(reply["stopping"], serde_json::json!(true));
    let reason = client.recv_until("exit message", |message| match message {
        ServerMessage::Exit { reason } => Some(reason.clone()),
        _ => None,
    });
    assert_eq!(reason, "shutdown");
    assert!(server.wait_exit().success());
    assert!(!scratch.socket("ctl").exists());
}

/// One opaque red RGBA pixel, base64.
const RED_PIXEL: &str = "/wAA/w==";

/// Shell input that prints a 1x1 RGBA kitty image with `a=T` and image id `id`.
fn print_image(id: u32, extra: &str) -> String {
    format!("printf '\\033_Ga=T,f=32,s=1,v=1,i={id}{extra};{RED_PIXEL}\\033\\\\'\n")
}

#[test]
fn kitty_image_reaches_the_client_once_with_placements_and_the_pty_gets_the_reply() {
    let scratch = Scratch::new();
    let _server = scratch.spawn("image");
    let mut client = scratch.attach_hello("image");
    let pane = client.focused_pane();
    // The reply is read back from the terminal and shown with od, so the test
    // sees exactly the bytes the server wrote to the pane PTY.
    let command = print_image(1, "").replace("printf", "stty -echo -icanon; printf");
    let command = format!(
        "{}; dd bs=1 count=11 2>/dev/null | od -An -c; stty sane\n",
        command.trim_end()
    );
    input(&mut client, pane, &command);
    client.wait_text(pane, "O   K");
    let text = client.text(pane);
    assert!(
        text.contains("033   _   G   i   =   1   ;   O   K 033"),
        "PTY reply: {text}"
    );
    wait_until_placed(&mut client, pane, 1);
    assert_eq!(client.image(pane, 1), Some((1, 1, &[255, 0, 0, 255][..])));
    assert_eq!(
        client.placements(pane),
        vec![FramePlacement {
            image: 1,
            column: client.placements(pane)[0].column,
            row: client.placements(pane)[0].row,
            // A square image in 8x16 px cells keeps its aspect over two columns.
            columns: 2,
            rows: 1,
            x_offset: 0,
            y_offset: 0,
            source_x: 0,
            source_y: 0,
            source_width: 1,
            source_height: 1,
            z: 0,
        }]
    );
    // More output re-sends frames but never the image again.
    input(&mut client, pane, "echo after-image-$((3+4))\n");
    client.wait_text(pane, "after-image-7");
    assert_eq!(client.placements(pane).len(), 1);
    assert_eq!(client.announcements, vec![(pane, 1)]);

    // An uppercase delete frees the data: placements go away and the client drops it.
    input(&mut client, pane, "printf '\\033_Ga=d,d=A\\033\\\\'\n");
    client.recv_until("image removal", |message| match message {
        ServerMessage::ImageRemoved { pane: p, image: 1 } if *p == pane => Some(()),
        _ => None,
    });
    while !client.placements(pane).is_empty() {
        client.recv_until("frame without placements", |message| match message {
            ServerMessage::Frame(frame) if frame.pane == pane => Some(()),
            _ => None,
        });
    }
    assert_eq!(client.removed, vec![(pane, 1)]);
    assert!(client.image(pane, 1).is_none());
}

/// Reads frames until `pane` shows a placement of `image`.
fn wait_until_placed(client: &mut Client, pane: u64, image: u32) {
    while !client
        .placements(pane)
        .iter()
        .any(|placement| placement.image == image)
    {
        client.recv_until("placement frame", |message| match message {
            ServerMessage::Frame(frame) if frame.pane == pane => Some(()),
            _ => None,
        });
    }
}

#[test]
fn reattach_resends_image_data_before_the_frames_that_place_it() {
    let scratch = Scratch::new();
    let mut server = scratch.spawn("reimage");
    let mut first = scratch.attach_hello("reimage");
    let pane = first.focused_pane();
    input(&mut first, pane, &print_image(2, ",q=2"));
    wait_until_placed(&mut first, pane, 2);
    assert_eq!(first.announcements, vec![(pane, 2)]);
    first.send(&ClientMessage::Detach);
    wait_until("detached stream close", || first.closed());
    assert!(server.is_running());

    let mut second = None;
    wait_until("re-attach", || {
        let (client, reply) = scratch.attach("reimage");
        let ok = matches!(reply.outcome, ControlOutcome::Success(_));
        if ok {
            second = Some(client);
        }
        ok
    });
    let mut second = second.unwrap();
    second.send(&HELLO);
    second.recv_until("welcome", |message| {
        matches!(message, ServerMessage::Welcome { .. }).then_some(())
    });
    // Client::apply asserts the announcement precedes the placing frame.
    wait_until_placed(&mut second, pane, 2);
    assert_eq!(second.announcements, vec![(pane, 2)]);
    assert_eq!(second.image(pane, 2), Some((1, 1, &[255, 0, 0, 255][..])));
}

/// Kills the attached client without `Detach` (like SIGTERM on the window)
/// and re-attaches with `hello`, returning the new client after `Welcome`.
fn reattach_after_kill(
    scratch: &Scratch,
    session: &str,
    first: Client,
    hello: &ClientMessage,
) -> Client {
    drop(first);
    let mut second = None;
    wait_until("re-attach", || {
        let (client, reply) = scratch.attach(session);
        let ok = matches!(reply.outcome, ControlOutcome::Success(_));
        if ok {
            second = Some(client);
        }
        ok
    });
    let mut second = second.unwrap();
    second.send(hello);
    second.recv_until("welcome", |message| {
        matches!(message, ServerMessage::Welcome { .. }).then_some(())
    });
    second
}

#[test]
fn reattach_after_a_killed_client_resends_a_multi_row_image_completely() {
    let scratch = Scratch::new();
    let _server = scratch.spawn("killimg");
    let mut first = scratch.attach_hello("killimg");
    let pane = first.focused_pane();
    let rgba = cat_image(&scratch, &mut first, pane, 3, 48, 48);
    wait_until_placed(&mut first, pane, 3);
    let placed = first.placements(pane);
    assert_eq!((placed[0].columns, placed[0].rows), (6, 3));
    let mut second = reattach_after_kill(&scratch, "killimg", first, &HELLO);
    // Client::apply asserts the announcement precedes the placing frame.
    wait_until_placed(&mut second, pane, 3);
    assert_eq!(second.placements(pane), placed);
    assert_eq!(second.image(pane, 3), Some((48, 48, rgba.as_slice())));
    assert_eq!(second.announcements, vec![(pane, 3)]);
}

#[test]
fn reattach_with_a_new_width_drops_placements_like_a_width_reflow() {
    // Why the GUI client waits for its window size to settle before `Hello`:
    // a different width reflows the panes and kitty placements are dropped.
    let scratch = Scratch::new();
    let _server = scratch.spawn("widthimg");
    let mut first = scratch.attach_hello("widthimg");
    let pane = first.focused_pane();
    cat_image(&scratch, &mut first, pane, 4, 48, 48);
    wait_until_placed(&mut first, pane, 4);
    let wider = ClientMessage::Hello {
        width: 1000,
        height: 480,
        cell_width: 8,
        cell_height: 16,
    };
    let mut second = reattach_after_kill(&scratch, "widthimg", first, &wider);
    let frame = second.recv_until("full frame", |message| match message {
        ServerMessage::Frame(frame) if frame.pane == pane => Some(frame.clone()),
        _ => None,
    });
    assert!(frame.full);
    assert_eq!(frame.cols, 125);
    assert!(frame.placements.is_empty());
    assert!(
        second.announcements.is_empty(),
        "unplaced images are not sent"
    );
}

/// Standard base64 through the attach codec (the crate has no direct base64 dependency).
fn base64_of(bytes: &[u8]) -> String {
    let line = encode_client(&ClientMessage::Input {
        pane: 0,
        data: bytes.to_vec(),
    })
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
    value["data"].as_str().unwrap().to_owned()
}

/// Makes the pane shell print a `width` x `height` RGBA kitty image (`a=T`,
/// `q=2`) from a file, so no shell input line approaches the tty line limit.
fn cat_image(
    scratch: &Scratch,
    client: &mut Client,
    pane: u64,
    id: u32,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let rgba: Vec<u8> = (0..width * height)
        .flat_map(|pixel| [(pixel % 251) as u8, 128, 255, 255])
        .collect();
    let sequence = format!(
        "\x1b_Ga=T,f=32,s={width},v={height},i={id},q=2;{}\x1b\\",
        base64_of(&rgba)
    );
    fs::write(scratch.root.join(format!("image-{id}.apc")), sequence).unwrap();
    input(
        client,
        pane,
        &format!("cat image-{id}.apc; echo; echo below-$((40+{id}))\n"),
    );
    client.wait_text(pane, &format!("below-{}", 40 + id));
    rgba
}

//! Attached GUI client of a `nulix-server` session (Phase 5 T11c).
//!
//! `nulix --session NAME` does not own PTYs or VT models. It connects to the
//! session server (starting it when absent, see [`ensure_server`]), upgrades a
//! control connection to an attach stream ([`AttachConnection`]), and renders
//! what the server reports. [`RemoteView`] is the pure client-side store: it
//! applies [`LayoutFrame`] and [`PaneFrame`] messages, enforces the per-pane
//! sequence rule documented on [`PaneFrame`], and says when a pane needs a
//! `Resync`. Each pane yields the same [`PaneRenderInput`] the standalone
//! window builds from its local model, so both modes share the GPU renderer.
//!
//! Kitty images (Phase 5 T11d): the view also reassembles the RGBA images the
//! server announces ([`ServerMessage::Image`]) from their chunks, checking the
//! announced size, chunk contiguity, the kitty dimension limit and a total of
//! [`MAX_CLIENT_IMAGE_BYTES`]. Only complete images are returned by
//! [`RemoteView::image`]; frame placements that reference an incomplete image
//! are skipped by the renderer until it completes. Images are dropped on
//! `ImageRemoved` and when their pane disappears from the layout.
//!
//! The view store is platform-independent; the transport needs Unix-domain
//! sockets, and on other platforms [`ensure_server`] and
//! [`AttachConnection::attach`] report that persistent sessions are unsupported.

use crate::PaneRenderInput;
use crate::attach::{
    ClientMessage, FramePlacement, LayoutFrame, LayoutTab, PaneFrame, PaneModes, ServerMessage,
    cells_from_runs, rgba_len, selection_cells,
};
use crate::graphics::{self, DecodedImage};
use crate::keyboard::KeyboardFlags;
use crate::layout::{PaneId, PaneLayout, Rect};
use crate::model::{CellColor, Cursor, VisibleCell};
use crate::renderer::BlockDecoration;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use vte::ansi::{CursorShape, NamedColor};

/// Most announced image bytes the client keeps at once, like the process image limit.
pub const MAX_CLIENT_IMAGE_BYTES: u64 = graphics::PROCESS_LIMIT as u64;

/// How long [`ensure_server`] waits for a freshly spawned server to listen.
pub const SERVER_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The cell a fresh VT grid holds: a space in the default colors.
pub fn blank_cell() -> VisibleCell {
    VisibleCell {
        character: ' ',
        foreground: CellColor::Named(NamedColor::Foreground as u16),
        background: CellColor::Named(NamedColor::Background as u16),
        bold: false,
        inverse: false,
    }
}

/// Rebuilds encoder flags from the raw kitty keyboard bits carried in frames.
pub fn keyboard_flags_from_bits(bits: u8) -> KeyboardFlags {
    [
        KeyboardFlags::DISAMBIGUATE,
        KeyboardFlags::REPORT_EVENT_TYPES,
        KeyboardFlags::REPORT_ALTERNATE,
        KeyboardFlags::REPORT_ALL,
        KeyboardFlags::REPORT_ASSOCIATED_TEXT,
    ]
    .into_iter()
    .filter(|flag| bits & flag.bits() != 0)
    .fold(KeyboardFlags::empty(), |flags, flag| flags | flag)
}

/// How long the window size must stay unchanged before the client attaches.
pub const ATTACH_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

/// Delays the attach until the window size has settled.
///
/// A new X11 window reports its requested size first, and the window manager
/// may resize it right after mapping. `Hello` geometry is authoritative: the
/// server reflows every visible pane to it, and a width reflow drops kitty
/// placements for good. Attaching with the provisional size would therefore
/// lose a session's images on re-attach even when the final size matches the
/// previous one, so the client sends `Hello` only once no resize happened for
/// [`ATTACH_SETTLE`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttachSettle {
    deadline: Option<std::time::Instant>,
}

impl AttachSettle {
    pub fn new(now: std::time::Instant) -> Self {
        Self {
            deadline: Some(now + ATTACH_SETTLE),
        }
    }

    /// A window resize before attaching restarts the wait.
    pub fn resized(&mut self, now: std::time::Instant) {
        if self.deadline.is_some() {
            self.deadline = Some(now + ATTACH_SETTLE);
        }
    }

    /// True once the size was stable for [`ATTACH_SETTLE`] and no attach happened yet.
    pub fn due(&self, now: std::time::Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// When to check again; `None` after attaching.
    pub fn deadline(&self) -> Option<std::time::Instant> {
        self.deadline
    }

    pub fn attached(&mut self) {
        self.deadline = None;
    }
}

/// Window pixel size and whole-pixel cell metrics sent in `Hello` and `Resize`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowMetrics {
    pub width: u32,
    pub height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
}

impl WindowMetrics {
    /// Cell metrics are rounded up, matching the window's local `CellSize`.
    pub fn new(width: u32, height: u32, cell_width: f32, cell_height: f32) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
            cell_width: (cell_width.ceil() as u32).max(1),
            cell_height: (cell_height.ceil() as u32).max(1),
        }
    }

    pub fn hello(self) -> ClientMessage {
        ClientMessage::Hello {
            width: self.width,
            height: self.height,
            cell_width: self.cell_width,
            cell_height: self.cell_height,
        }
    }

    pub fn resize(self) -> ClientMessage {
        ClientMessage::Resize {
            width: self.width,
            height: self.height,
            cell_width: self.cell_width,
            cell_height: self.cell_height,
        }
    }
}

/// Client copy of one server pane, rebuilt from its frames.
#[derive(Clone, Debug, PartialEq)]
pub struct RemotePane {
    pub cols: usize,
    pub rows: usize,
    /// Exactly `rows` rows of exactly `cols` cells.
    pub cells: Vec<Vec<VisibleCell>>,
    pub cursor: Cursor,
    /// Selected cells as `(column, row)`.
    pub selected: Vec<(usize, usize)>,
    pub modes: PaneModes,
    pub decorations: Vec<BlockDecoration>,
    pub title: String,
    pub cwd: String,
    /// Kitty image placements of the latest frame, in drawing order.
    pub placements: Vec<FramePlacement>,
    seq: u64,
    awaiting_full: bool,
}

impl RemotePane {
    fn awaiting(seq: u64) -> Self {
        Self {
            cols: 0,
            rows: 0,
            cells: Vec::new(),
            cursor: Cursor {
                column: 0,
                row: 0,
                shape: CursorShape::Hidden,
            },
            selected: Vec::new(),
            modes: PaneModes::default(),
            decorations: Vec::new(),
            title: String::new(),
            cwd: String::new(),
            placements: Vec::new(),
            seq,
            awaiting_full: true,
        }
    }

    /// True after a sequence gap until the next full frame arrives.
    pub fn awaiting_full(&self) -> bool {
        self.awaiting_full
    }

    pub fn keyboard_flags(&self) -> KeyboardFlags {
        keyboard_flags_from_bits(self.modes.keyboard_flags)
    }

    /// The renderer input shared with the standalone window.
    pub fn render_input(&self) -> PaneRenderInput {
        PaneRenderInput {
            rows: self.cells.clone(),
            cursor: (self.cursor.shape != CursorShape::Hidden)
                .then_some((self.cursor.column, self.cursor.row)),
            selected: self.selected.clone(),
            decorations: self.decorations.clone(),
        }
    }

    fn set_row(&mut self, row: usize, cells: Vec<VisibleCell>) {
        let Some(target) = self.cells.get_mut(row) else {
            return;
        };
        let mut cells = cells;
        cells.resize(self.cols, blank_cell());
        *target = cells;
    }

    fn apply_complete_state(&mut self, frame: PaneFrame) {
        self.cursor = frame.cursor;
        self.selected = selection_cells(&frame.selection);
        self.modes = frame.modes;
        self.decorations = frame.decorations;
        self.title = frame.title;
        self.cwd = frame.cwd;
        self.placements = frame.placements;
        self.seq = frame.seq;
    }
}

/// What applying one frame did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameOutcome {
    /// The pane view changed.
    Applied,
    /// The frame was dropped while the pane waits for a full frame.
    Ignored,
    /// A sequence gap (or unusable delta) was detected; send `Resync{pane}` once.
    ResyncNeeded,
}

/// Effect of one server message on the view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ViewChange {
    /// The message does not change the view.
    None,
    /// The layout was replaced; `removed` panes no longer exist.
    Layout {
        removed: Vec<u64>,
    },
    Frame {
        pane: u64,
        outcome: FrameOutcome,
    },
    /// An image completed, was removed, or was rejected.
    Image {
        pane: u64,
        image: u32,
        change: ImageChange,
    },
}

/// What happened to one image of the view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImageChange {
    /// Every byte arrived; [`RemoteView::image`] now returns it.
    Ready,
    /// The image was removed.
    Removed,
    /// The image was dropped because its data broke the protocol bounds.
    Rejected(String),
}

/// One image being received, or complete.
#[derive(Clone, Debug)]
enum RemoteImage {
    Receiving {
        width: u32,
        height: u32,
        bytes: u64,
        data: Vec<u8>,
    },
    Ready(DecodedImage),
}

impl RemoteImage {
    fn announced_bytes(&self) -> u64 {
        match self {
            Self::Receiving { bytes, .. } => *bytes,
            Self::Ready(image) => image.rgba.len() as u64,
        }
    }
}

/// Pure client-side store of the server's layout, pane frames and images.
#[derive(Clone, Debug, Default)]
pub struct RemoteView {
    layout: Option<LayoutFrame>,
    panes: BTreeMap<u64, RemotePane>,
    images: BTreeMap<(u64, u32), RemoteImage>,
    /// Sum of announced bytes of every kept image.
    image_bytes: u64,
    /// Images dropped for a protocol violation; their remaining chunks are ignored.
    rejected: std::collections::BTreeSet<(u64, u32)>,
}

impl RemoteView {
    /// Applies a layout, frame or image message; other messages are not view state.
    pub fn apply(&mut self, message: ServerMessage) -> ViewChange {
        match message {
            ServerMessage::Image {
                pane,
                image,
                width,
                height,
                bytes,
            } => self.announce_image(pane, image, width, height, bytes),
            ServerMessage::ImageData {
                pane,
                image,
                offset,
                data,
            } => self.receive_image_data(pane, image, offset, data),
            ServerMessage::ImageRemoved { pane, image } => {
                self.rejected.remove(&(pane, image));
                if self.remove_image(pane, image) {
                    ViewChange::Image {
                        pane,
                        image,
                        change: ImageChange::Removed,
                    }
                } else {
                    ViewChange::None
                }
            }
            ServerMessage::Layout(layout) => ViewChange::Layout {
                removed: self.apply_layout(layout),
            },
            ServerMessage::Frame(frame) => {
                let pane = frame.pane;
                ViewChange::Frame {
                    pane,
                    outcome: self.apply_frame(frame),
                }
            }
            _ => ViewChange::None,
        }
    }

    /// Replaces the layout and forgets panes that no tab lists any more.
    pub fn apply_layout(&mut self, layout: LayoutFrame) -> Vec<u64> {
        let live: std::collections::BTreeSet<u64> = layout
            .tabs
            .iter()
            .flat_map(|tab| tab.panes.iter().map(|pane| pane.pane))
            .collect();
        let removed: Vec<u64> = self
            .panes
            .keys()
            .copied()
            .filter(|pane| !live.contains(pane))
            .collect();
        for pane in &removed {
            self.panes.remove(pane);
        }
        let gone: Vec<(u64, u32)> = self
            .images
            .keys()
            .chain(self.rejected.iter())
            .copied()
            .filter(|(pane, _)| !live.contains(pane))
            .collect();
        for (pane, image) in gone {
            self.remove_image(pane, image);
            self.rejected.remove(&(pane, image));
        }
        self.layout = Some(layout);
        removed
    }

    /// Complete RGBA data of a pane's image; `None` while chunks are in flight.
    pub fn image(&self, pane: u64, image: u32) -> Option<&DecodedImage> {
        match self.images.get(&(pane, image))? {
            RemoteImage::Ready(decoded) => Some(decoded),
            RemoteImage::Receiving { .. } => None,
        }
    }

    fn remove_image(&mut self, pane: u64, image: u32) -> bool {
        match self.images.remove(&(pane, image)) {
            Some(removed) => {
                self.image_bytes -= removed.announced_bytes();
                true
            }
            None => false,
        }
    }

    fn reject(&mut self, pane: u64, image: u32, reason: String) -> ViewChange {
        self.remove_image(pane, image);
        self.rejected.insert((pane, image));
        ViewChange::Image {
            pane,
            image,
            change: ImageChange::Rejected(reason),
        }
    }

    /// Starts (or restarts) receiving an image; the previous data is replaced.
    fn announce_image(
        &mut self,
        pane: u64,
        image: u32,
        width: u32,
        height: u32,
        bytes: u64,
    ) -> ViewChange {
        self.remove_image(pane, image);
        self.rejected.remove(&(pane, image));
        if width == 0
            || height == 0
            || width > graphics::DIMENSION_LIMIT
            || height > graphics::DIMENSION_LIMIT
            || rgba_len(width, height) != Some(bytes)
        {
            return self.reject(
                pane,
                image,
                format!("image {image} of pane {pane} has invalid dimensions {width}x{height}"),
            );
        }
        if self.image_bytes.saturating_add(bytes) > MAX_CLIENT_IMAGE_BYTES {
            return self.reject(
                pane,
                image,
                format!("image {image} of pane {pane} exceeds the 512 MiB client image limit"),
            );
        }
        self.image_bytes += bytes;
        self.images.insert(
            (pane, image),
            RemoteImage::Receiving {
                width,
                height,
                bytes,
                data: Vec::new(),
            },
        );
        ViewChange::None
    }

    /// Appends one chunk; completes the image after its last byte.
    fn receive_image_data(
        &mut self,
        pane: u64,
        image: u32,
        offset: u64,
        chunk: Vec<u8>,
    ) -> ViewChange {
        if self.rejected.contains(&(pane, image)) {
            return ViewChange::None;
        }
        let Some(RemoteImage::Receiving {
            width,
            height,
            bytes,
            data,
        }) = self.images.get_mut(&(pane, image))
        else {
            return self.reject(
                pane,
                image,
                format!("image data for unannounced image {image} of pane {pane}"),
            );
        };
        if offset != data.len() as u64 {
            return self.reject(
                pane,
                image,
                format!("image {image} of pane {pane}: chunk at {offset} is not contiguous"),
            );
        }
        if offset.saturating_add(chunk.len() as u64) > *bytes {
            return self.reject(
                pane,
                image,
                format!("image {image} of pane {pane}: chunk exceeds the announced size"),
            );
        }
        data.extend_from_slice(&chunk);
        if (data.len() as u64) < *bytes {
            return ViewChange::None;
        }
        let (width, height, rgba) = (*width, *height, std::mem::take(data));
        match DecodedImage::new(width, height, rgba) {
            Ok(decoded) => {
                self.images
                    .insert((pane, image), RemoteImage::Ready(decoded));
                ViewChange::Image {
                    pane,
                    image,
                    change: ImageChange::Ready,
                }
            }
            Err(_) => self.reject(
                pane,
                image,
                format!("image {image} of pane {pane} is not valid RGBA data"),
            ),
        }
    }

    /// Applies one frame following the [`PaneFrame`] sequence rule.
    ///
    /// A full frame always replaces the pane view, blanking unlisted rows. A
    /// delta applies only when its seq is the previous seq plus one and its
    /// grid size is unchanged; otherwise the pane waits for a full frame and
    /// reports [`FrameOutcome::ResyncNeeded`] once, ignoring later deltas.
    pub fn apply_frame(&mut self, frame: PaneFrame) -> FrameOutcome {
        if frame.full {
            let id = frame.pane;
            let mut pane = RemotePane::awaiting(frame.seq);
            pane.cols = frame.cols;
            pane.rows = frame.rows;
            pane.cells = vec![vec![blank_cell(); frame.cols]; frame.rows];
            pane.awaiting_full = false;
            for line in &frame.lines {
                pane.set_row(line.row, cells_from_runs(&line.runs));
            }
            pane.apply_complete_state(frame);
            self.panes.insert(id, pane);
            return FrameOutcome::Applied;
        }
        let Some(pane) = self.panes.get_mut(&frame.pane) else {
            self.panes
                .insert(frame.pane, RemotePane::awaiting(frame.seq));
            return FrameOutcome::ResyncNeeded;
        };
        if pane.awaiting_full {
            return FrameOutcome::Ignored;
        }
        let contiguous = pane.seq.checked_add(1) == Some(frame.seq);
        if !contiguous || frame.cols != pane.cols || frame.rows != pane.rows {
            pane.awaiting_full = true;
            return FrameOutcome::ResyncNeeded;
        }
        for line in &frame.lines {
            pane.set_row(line.row, cells_from_runs(&line.runs));
        }
        pane.apply_complete_state(frame);
        FrameOutcome::Applied
    }

    pub fn layout(&self) -> Option<&LayoutFrame> {
        self.layout.as_ref()
    }

    pub fn pane(&self, pane: u64) -> Option<&RemotePane> {
        self.panes.get(&pane)
    }

    fn active_tab(&self) -> Option<&LayoutTab> {
        let layout = self.layout.as_ref()?;
        layout.tabs.get(layout.active_tab)
    }

    pub fn tab_count(&self) -> usize {
        self.layout.as_ref().map_or(0, |layout| layout.tabs.len())
    }

    pub fn active_tab_index(&self) -> usize {
        self.layout.as_ref().map_or(0, |layout| layout.active_tab)
    }

    pub fn tab_titles(&self) -> Vec<String> {
        self.layout.as_ref().map_or_else(Vec::new, |layout| {
            layout.tabs.iter().map(|tab| tab.title.clone()).collect()
        })
    }

    /// Label of the active tab, used as the window title.
    pub fn active_title(&self) -> Option<&str> {
        self.active_tab().map(|tab| tab.title.as_str())
    }

    /// The focused pane of the active tab.
    pub fn focused_pane(&self) -> Option<PaneId> {
        self.active_tab()?
            .panes
            .iter()
            .find(|pane| pane.focused)
            .map(|pane| PaneId(pane.pane))
    }

    /// Active-tab pane geometry in the window's local layout type, so pointer
    /// hit-testing and rendering use the same code in both window modes.
    pub fn pane_layouts(&self) -> Vec<PaneLayout> {
        self.active_tab().map_or_else(Vec::new, |tab| {
            tab.panes
                .iter()
                .map(|pane| PaneLayout {
                    pane: PaneId(pane.pane),
                    rect: Rect::new(pane.x, pane.y, pane.width, pane.height),
                    cols: pane.cols.min(u32::MAX as usize) as u32,
                    rows: pane.rows.min(u32::MAX as usize) as u32,
                    hidden: pane.hidden,
                })
                .collect()
        })
    }

    /// Divider strips between adjacent visible panes of the active tab.
    ///
    /// The split tree lives on the server, so dividers are derived from the
    /// gaps between pane rectangles that face each other: each strip spans the
    /// overlap of the two facing edges.
    pub fn divider_rects(&self) -> Vec<Rect> {
        let visible: Vec<Rect> = self
            .pane_layouts()
            .into_iter()
            .filter(|layout| !layout.hidden)
            .map(|layout| layout.rect)
            .collect();
        let mut dividers = Vec::new();
        for a in &visible {
            let nearest_right = visible
                .iter()
                .filter(|b| b.x > a.right() && overlap(a.y, a.bottom(), b.y, b.bottom()).is_some())
                .map(|b| b.x)
                .min();
            if let Some(x) = nearest_right {
                for b in visible.iter().filter(|b| b.x == x) {
                    if let Some((top, bottom)) = overlap(a.y, a.bottom(), b.y, b.bottom()) {
                        push_unique(
                            &mut dividers,
                            Rect::new(a.right(), top, x - a.right(), bottom - top),
                        );
                    }
                }
            }
            let nearest_below = visible
                .iter()
                .filter(|b| b.y > a.bottom() && overlap(a.x, a.right(), b.x, b.right()).is_some())
                .map(|b| b.y)
                .min();
            if let Some(y) = nearest_below {
                for b in visible.iter().filter(|b| b.y == y) {
                    if let Some((left, right)) = overlap(a.x, a.right(), b.x, b.right()) {
                        push_unique(
                            &mut dividers,
                            Rect::new(left, a.bottom(), right - left, y - a.bottom()),
                        );
                    }
                }
            }
        }
        dividers
    }
}

fn overlap(a_start: u32, a_end: u32, b_start: u32, b_end: u32) -> Option<(u32, u32)> {
    let start = a_start.max(b_start);
    let end = a_end.min(b_end);
    (start < end).then_some((start, end))
}

fn push_unique(rects: &mut Vec<Rect>, rect: Rect) {
    if !rects.contains(&rect) {
        rects.push(rect);
    }
}

/// `nulix-server` next to `current_exe` when present, otherwise the bare name
/// so the spawn searches `PATH`.
pub fn server_program(current_exe: Option<&Path>) -> PathBuf {
    let name = format!("nulix-server{}", std::env::consts::EXE_SUFFIX);
    current_exe
        .and_then(Path::parent)
        .map(|dir| dir.join(&name))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Event delivered by the attach reader thread.
#[derive(Clone, Debug, PartialEq)]
pub enum RemoteEvent {
    Message(ServerMessage),
    /// The stream ended: `None` on a clean end of stream, otherwise a diagnostic.
    Closed(Option<String>),
}

#[cfg(unix)]
mod transport {
    use super::{RemoteEvent, SERVER_START_TIMEOUT, WindowMetrics, server_program};
    use crate::attach::{ClientMessage, MAX_ATTACH_LINE_BYTES, encode_client, parse_server};
    use crate::control::{
        self, ControlCommand, ControlOutcome, ControlRequest, MAX_REQUEST_LINE_BYTES,
        parse_response, serialize_request,
    };
    use std::{
        io::{self, BufRead, BufReader, Read, Write},
        os::unix::{net::UnixStream, process::CommandExt},
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::Mutex,
        thread,
        time::{Duration, Instant},
    };

    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    /// Returns the socket of session `session`, starting `nulix-server` when
    /// no live server answers on it.
    ///
    /// The server binary is the `nulix-server` next to the current
    /// executable, falling back to `PATH`. It runs in a new session (`setsid`)
    /// with null stdio so it outlives this window and its terminal.
    pub fn ensure_server(session: &str) -> io::Result<PathBuf> {
        let directory = control::runtime_directory()?;
        let exe = std::env::current_exe().ok();
        ensure_server_in(
            &directory,
            session,
            &server_program(exe.as_deref()),
            SERVER_START_TIMEOUT,
        )
    }

    /// [`ensure_server`] with an explicit runtime directory, server program and timeout.
    pub fn ensure_server_in(
        directory: &Path,
        session: &str,
        program: &Path,
        timeout: Duration,
    ) -> io::Result<PathBuf> {
        control::validate_session_name(session)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let socket = directory.join(control::session_socket_name(session));
        if UnixStream::connect(&socket).is_ok() {
            return Ok(socket);
        }
        let mut command = Command::new(program);
        command
            .arg("--session")
            .arg(session)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe and touches no parent state.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not start {}: {error}", program.display()),
            )
        })?;
        let deadline = Instant::now() + timeout;
        loop {
            if UnixStream::connect(&socket).is_ok() {
                // Reap the detached server whenever it ends so it never lingers as a zombie.
                thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(socket);
            }
            if let Some(status) = child.try_wait()? {
                // A concurrent client may have started the same session first.
                if UnixStream::connect(&socket).is_ok() {
                    return Ok(socket);
                }
                return Err(io::Error::other(format!(
                    "nulix-server for session {session} exited before its socket accepted connections ({status})"
                )));
            }
            if Instant::now() >= deadline {
                thread::spawn(move || {
                    let _ = child.wait();
                });
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "nulix-server for session {session} did not accept connections within {} ms",
                        timeout.as_millis()
                    ),
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Client end of an attach stream.
    pub struct AttachConnection {
        writer: Mutex<UnixStream>,
        session: String,
    }

    impl AttachConnection {
        /// Sends the control `attach` request, checks the success reply, sends
        /// `Hello` with `metrics`, and starts one reader thread that hands every
        /// server message (and finally the stream end) to `on_event`.
        pub fn attach<F>(socket: &Path, metrics: WindowMetrics, on_event: F) -> io::Result<Self>
        where
            F: FnMut(RemoteEvent) + Send + 'static,
        {
            let mut stream = UnixStream::connect(socket)?;
            stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
            let request = ControlRequest {
                id: 1,
                command: ControlCommand::Attach,
            };
            stream.write_all(
                &serialize_request(&request)
                    .map_err(|error| io::Error::other(error.to_string()))?,
            )?;
            // The reader keeps any stream bytes buffered after the control reply.
            let mut reader = BufReader::new(stream.try_clone()?);
            let mut line = Vec::new();
            (&mut reader)
                .take((MAX_REQUEST_LINE_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)?;
            if line.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "session server closed the connection before answering attach",
                ));
            }
            let response = parse_response(&line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            let session = match response.outcome {
                ControlOutcome::Success(value)
                    if value.get("attached").and_then(|v| v.as_bool()) == Some(true) =>
                {
                    value
                        .get("session")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned()
                }
                ControlOutcome::Success(value) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected attach reply {value}"),
                    ));
                }
                ControlOutcome::Failure(error) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        format!("attach refused: {}", error.message),
                    ));
                }
            };
            stream.set_read_timeout(None)?;
            let connection = Self {
                writer: Mutex::new(stream),
                session,
            };
            connection.send(&metrics.hello())?;
            thread::Builder::new()
                .name("nulix-attach-reader".into())
                .spawn(move || read_stream(reader, on_event))?;
            Ok(connection)
        }

        /// Session name confirmed by the server.
        pub fn session(&self) -> &str {
            &self.session
        }

        pub fn send(&self, message: &ClientMessage) -> io::Result<()> {
            let bytes =
                encode_client(message).map_err(|error| io::Error::other(error.to_string()))?;
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| io::Error::other("attach writer lock poisoned"))?;
            writer.write_all(&bytes)
        }

        /// Sends `Detach`; the server keeps the session running and closes the stream.
        pub fn detach(&self) -> io::Result<()> {
            self.send(&ClientMessage::Detach)
        }
    }

    fn read_stream<F>(mut reader: BufReader<UnixStream>, mut on_event: F)
    where
        F: FnMut(RemoteEvent),
    {
        loop {
            let mut line = Vec::new();
            match (&mut reader)
                .take((MAX_ATTACH_LINE_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)
            {
                Ok(0) => {
                    on_event(RemoteEvent::Closed(None));
                    return;
                }
                Ok(_) => match parse_server(&line) {
                    Ok(message) => on_event(RemoteEvent::Message(message)),
                    Err(error) => {
                        on_event(RemoteEvent::Closed(Some(format!(
                            "invalid attach message from server: {error}"
                        ))));
                        return;
                    }
                },
                Err(error) => {
                    on_event(RemoteEvent::Closed(Some(format!(
                        "attach stream read failed: {error}"
                    ))));
                    return;
                }
            }
        }
    }
}

#[cfg(unix)]
pub use transport::{AttachConnection, ensure_server, ensure_server_in};

#[cfg(not(unix))]
mod unsupported {
    use super::{RemoteEvent, WindowMetrics};
    use crate::attach::ClientMessage;
    use std::{io, path::Path, path::PathBuf};

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "persistent sessions are unsupported on this platform",
        )
    }

    pub fn ensure_server(_session: &str) -> io::Result<PathBuf> {
        Err(unsupported())
    }

    /// Placeholder so window code compiles; it can never be constructed.
    pub struct AttachConnection {
        _never: std::convert::Infallible,
    }

    impl AttachConnection {
        pub fn attach<F>(_socket: &Path, _metrics: WindowMetrics, _on_event: F) -> io::Result<Self>
        where
            F: FnMut(RemoteEvent) + Send + 'static,
        {
            Err(unsupported())
        }

        pub fn session(&self) -> &str {
            match self._never {}
        }

        pub fn send(&self, _message: &ClientMessage) -> io::Result<()> {
            match self._never {}
        }

        pub fn detach(&self) -> io::Result<()> {
            match self._never {}
        }
    }
}

#[cfg(not(unix))]
pub use unsupported::{AttachConnection, ensure_server};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::{
        FrameLine, LayoutFrame, LayoutPane, LayoutTab, PaneFrame, PaneModes, SelectionSpan,
        ServerMessage, line_runs,
    };
    use crate::layout::{PaneId, Rect};
    use crate::model::{CellColor, Cursor, VisibleCell};
    use crate::renderer::{BlockDecoration, BlockDecorationColor};
    use vte::ansi::CursorShape;

    fn text_cells(text: &str) -> Vec<VisibleCell> {
        text.chars()
            .map(|character| VisibleCell {
                character,
                ..blank_cell()
            })
            .collect()
    }

    fn line(row: usize, text: &str) -> FrameLine {
        FrameLine {
            row,
            runs: line_runs(&text_cells(text)),
        }
    }

    fn frame(pane: u64, seq: u64, full: bool, lines: Vec<FrameLine>) -> PaneFrame {
        PaneFrame {
            pane,
            seq,
            full,
            cols: 4,
            rows: 3,
            lines,
            cursor: Cursor {
                column: 1,
                row: 2,
                shape: CursorShape::Block,
            },
            selection: Vec::new(),
            modes: PaneModes::default(),
            decorations: Vec::new(),
            title: "title".into(),
            cwd: "/tmp".into(),
            placements: Vec::new(),
        }
    }

    fn row_text(view: &RemoteView, pane: u64, row: usize) -> String {
        view.pane(pane).unwrap().cells[row]
            .iter()
            .map(|cell| cell.character)
            .collect()
    }

    fn layout_pane(pane: u64, x: u32, width: u32, focused: bool) -> LayoutPane {
        LayoutPane {
            pane,
            x,
            y: 16,
            width,
            height: 160,
            cols: (width / 8) as usize,
            rows: 10,
            hidden: false,
            focused,
        }
    }

    fn two_tab_layout(revision: u64) -> LayoutFrame {
        LayoutFrame {
            revision,
            active_tab: 1,
            tabs: vec![
                LayoutTab {
                    title: "first".into(),
                    panes: vec![layout_pane(1, 0, 320, true)],
                },
                LayoutTab {
                    title: "second".into(),
                    panes: vec![
                        layout_pane(2, 0, 160, false),
                        layout_pane(3, 165, 155, true),
                    ],
                },
            ],
        }
    }

    #[test]
    fn full_frame_replaces_the_pane_view_and_blanks_unlisted_rows() {
        let mut view = RemoteView::default();
        let mut first = frame(7, 1, true, vec![line(0, "ab"), line(2, "wxyz")]);
        first.selection = vec![SelectionSpan {
            row: 0,
            start: 0,
            end: 1,
        }];
        first.modes = PaneModes {
            keyboard_flags: 3,
            app_cursor: true,
            bracketed_paste: true,
            mouse_or_alternate: false,
            alternate_screen: false,
        };
        first.decorations = vec![BlockDecoration {
            block_id: 4,
            marker_row: 0,
            separator_row: Some(2),
            color: BlockDecorationColor::Success,
        }];
        assert_eq!(view.apply_frame(first), FrameOutcome::Applied);
        let pane = view.pane(7).unwrap();
        assert_eq!((pane.cols, pane.rows), (4, 3));
        assert_eq!(pane.cells.len(), 3);
        assert!(pane.cells.iter().all(|row| row.len() == 4));
        assert_eq!(row_text(&view, 7, 0), "ab  ");
        assert_eq!(row_text(&view, 7, 1), "    ");
        assert_eq!(row_text(&view, 7, 2), "wxyz");
        assert_eq!(pane.cursor.column, 1);
        assert_eq!(pane.selected, vec![(0, 0), (1, 0)]);
        assert!(pane.modes.app_cursor);
        assert_eq!(pane.decorations.len(), 1);
        assert_eq!(pane.title, "title");
        assert_eq!(pane.cwd, "/tmp");

        // A later full frame starts over: rows it does not list are blank again.
        assert_eq!(
            view.apply_frame(frame(7, 2, true, vec![line(1, "q")])),
            FrameOutcome::Applied
        );
        assert_eq!(row_text(&view, 7, 0), "    ");
        assert_eq!(row_text(&view, 7, 1), "q   ");
        assert_eq!(row_text(&view, 7, 2), "    ");
        assert!(view.pane(7).unwrap().selected.is_empty());
    }

    #[test]
    fn delta_frame_replaces_only_listed_rows_and_updates_complete_state() {
        let mut view = RemoteView::default();
        view.apply_frame(frame(1, 1, true, vec![line(0, "aaaa"), line(1, "bbbb")]));
        let mut delta = frame(1, 2, false, vec![line(1, "cc")]);
        delta.cursor.column = 3;
        delta.title = "renamed".into();
        delta.selection = vec![SelectionSpan {
            row: 2,
            start: 1,
            end: 2,
        }];
        assert_eq!(view.apply_frame(delta), FrameOutcome::Applied);
        assert_eq!(row_text(&view, 1, 0), "aaaa");
        assert_eq!(row_text(&view, 1, 1), "cc  ");
        let pane = view.pane(1).unwrap();
        assert_eq!(pane.cursor.column, 3);
        assert_eq!(pane.title, "renamed");
        assert_eq!(pane.selected, vec![(1, 2), (2, 2)]);
    }

    #[test]
    fn sequence_gap_requests_resync_and_ignores_deltas_until_the_next_full_frame() {
        let mut view = RemoteView::default();
        view.apply_frame(frame(1, 1, true, vec![line(0, "base")]));
        // seq 3 skips seq 2: the delta is not applied and a resync is needed once.
        assert_eq!(
            view.apply_frame(frame(1, 3, false, vec![line(0, "gap!")])),
            FrameOutcome::ResyncNeeded
        );
        assert_eq!(row_text(&view, 1, 0), "base");
        // Further deltas, even contiguous ones, are ignored without asking again.
        assert_eq!(
            view.apply_frame(frame(1, 4, false, vec![line(0, "late")])),
            FrameOutcome::Ignored
        );
        assert_eq!(row_text(&view, 1, 0), "base");
        assert!(view.pane(1).unwrap().awaiting_full());
        // The full frame restores the stream; the next contiguous delta applies.
        assert_eq!(
            view.apply_frame(frame(1, 5, true, vec![line(0, "full")])),
            FrameOutcome::Applied
        );
        assert!(!view.pane(1).unwrap().awaiting_full());
        assert_eq!(
            view.apply_frame(frame(1, 6, false, vec![line(0, "next")])),
            FrameOutcome::Applied
        );
        assert_eq!(row_text(&view, 1, 0), "next");
    }

    #[test]
    fn delta_for_an_unknown_pane_or_with_changed_geometry_needs_resync() {
        let mut view = RemoteView::default();
        assert_eq!(
            view.apply_frame(frame(9, 1, false, vec![line(0, "x")])),
            FrameOutcome::ResyncNeeded
        );
        assert_eq!(
            view.apply_frame(frame(9, 2, false, vec![line(0, "x")])),
            FrameOutcome::Ignored
        );
        view.apply_frame(frame(9, 3, true, Vec::new()));
        let mut resized = frame(9, 4, false, vec![line(0, "x")]);
        resized.cols = 10;
        assert_eq!(view.apply_frame(resized), FrameOutcome::ResyncNeeded);
        assert_eq!(view.pane(9).unwrap().cols, 4);
    }

    #[test]
    fn rows_outside_the_grid_are_ignored_and_long_rows_are_truncated() {
        let mut view = RemoteView::default();
        view.apply_frame(frame(
            1,
            1,
            true,
            vec![line(0, "abcdefgh"), line(5, "zzzz")],
        ));
        assert_eq!(row_text(&view, 1, 0), "abcd");
        assert_eq!(view.pane(1).unwrap().cells.len(), 3);
    }

    #[test]
    fn layout_replacement_exposes_the_active_tab_and_drops_vanished_panes() {
        let mut view = RemoteView::default();
        assert_eq!(view.apply_layout(two_tab_layout(1)), Vec::<u64>::new());
        for pane in [1, 2, 3] {
            view.apply_frame(frame(pane, 1, true, Vec::new()));
        }
        assert_eq!(view.focused_pane(), Some(PaneId(3)));
        assert_eq!(view.tab_count(), 2);
        assert_eq!(view.active_tab_index(), 1);
        assert_eq!(view.tab_titles(), vec!["first", "second"]);
        assert_eq!(view.active_title(), Some("second"));
        let layouts = view.pane_layouts();
        assert_eq!(layouts.len(), 2);
        assert_eq!(layouts[0].pane, PaneId(2));
        assert_eq!(layouts[0].rect, Rect::new(0, 16, 160, 160));
        assert_eq!((layouts[0].cols, layouts[0].rows), (20, 10));
        assert!(!layouts[1].hidden);

        let replacement = LayoutFrame {
            revision: 2,
            active_tab: 0,
            tabs: vec![LayoutTab {
                title: "only".into(),
                panes: vec![LayoutPane {
                    hidden: true,
                    ..layout_pane(1, 0, 320, true)
                }],
            }],
        };
        let mut removed = view.apply_layout(replacement);
        removed.sort_unstable();
        assert_eq!(removed, vec![2, 3]);
        assert!(view.pane(2).is_none());
        assert!(view.pane(1).is_some());
        assert_eq!(view.tab_count(), 1);
        assert!(view.pane_layouts()[0].hidden);
        assert_eq!(view.focused_pane(), Some(PaneId(1)));
    }

    #[test]
    fn dividers_fill_the_gap_between_adjacent_visible_panes() {
        let mut view = RemoteView::default();
        view.apply_layout(two_tab_layout(1));
        assert_eq!(view.divider_rects(), vec![Rect::new(160, 16, 5, 160)]);
        let stacked = LayoutFrame {
            revision: 2,
            active_tab: 0,
            tabs: vec![LayoutTab {
                title: "t".into(),
                panes: vec![
                    LayoutPane {
                        y: 0,
                        height: 100,
                        ..layout_pane(1, 0, 320, true)
                    },
                    LayoutPane {
                        y: 104,
                        height: 96,
                        ..layout_pane(2, 0, 320, false)
                    },
                ],
            }],
        };
        view.apply_layout(stacked);
        assert_eq!(view.divider_rects(), vec![Rect::new(0, 100, 320, 4)]);
    }

    #[test]
    fn selection_spans_expand_to_cells_and_feed_the_shared_render_input() {
        let mut view = RemoteView::default();
        let mut full = frame(1, 1, true, vec![line(0, "abcd")]);
        full.selection = vec![
            SelectionSpan {
                row: 0,
                start: 1,
                end: 3,
            },
            SelectionSpan {
                row: 1,
                start: 0,
                end: 0,
            },
        ];
        view.apply_frame(full);
        let pane = view.pane(1).unwrap();
        assert_eq!(pane.selected, vec![(1, 0), (2, 0), (3, 0), (0, 1)]);
        let input = pane.render_input();
        assert_eq!(input.selected, pane.selected);
        assert_eq!(input.cursor, Some((1, 2)));
        assert_eq!(input.rows, pane.cells);
    }

    #[test]
    fn hidden_cursor_shape_produces_no_render_cursor() {
        let mut view = RemoteView::default();
        let mut full = frame(1, 1, true, Vec::new());
        full.cursor.shape = CursorShape::Hidden;
        view.apply_frame(full);
        assert_eq!(view.pane(1).unwrap().render_input().cursor, None);
    }

    #[test]
    fn keyboard_flag_bits_round_trip_through_the_encoder_flags() {
        for bits in 0..32u8 {
            assert_eq!(keyboard_flags_from_bits(bits).bits(), bits);
        }
        assert_eq!(keyboard_flags_from_bits(0xff).bits(), 31);
    }

    #[test]
    fn blank_cell_matches_the_model_default_cell() {
        let model = crate::model::Model::new(1, 1);
        assert_eq!(model.visible_cells()[0][0], blank_cell());
        assert_eq!(blank_cell().foreground, CellColor::Named(256));
    }

    #[test]
    fn server_message_dispatch_reports_layout_and_frame_effects() {
        let mut view = RemoteView::default();
        assert_eq!(
            view.apply(ServerMessage::Layout(two_tab_layout(1))),
            ViewChange::Layout {
                removed: Vec::new()
            }
        );
        assert_eq!(
            view.apply(ServerMessage::Frame(frame(2, 1, true, Vec::new()))),
            ViewChange::Frame {
                pane: 2,
                outcome: FrameOutcome::Applied
            }
        );
        assert_eq!(
            view.apply(ServerMessage::Welcome {
                session: "s".into()
            }),
            ViewChange::None
        );
    }

    fn placement(image: u32, z: i32) -> FramePlacement {
        FramePlacement {
            image,
            column: 1,
            row: -1,
            columns: 2,
            rows: 3,
            x_offset: 0,
            y_offset: 1,
            source_x: 0,
            source_y: 0,
            source_width: 4,
            source_height: 4,
            z,
        }
    }

    /// Sends `messages` through the wire codec into `view`, returning each change.
    fn deliver(view: &mut RemoteView, messages: Vec<ServerMessage>) -> Vec<ViewChange> {
        messages
            .into_iter()
            .map(|message| {
                let line = crate::attach::encode_server(&message).unwrap();
                view.apply(crate::attach::parse_server(&line).unwrap())
            })
            .collect()
    }

    fn rejected(change: &ViewChange) -> bool {
        matches!(
            change,
            ViewChange::Image {
                change: ImageChange::Rejected(_),
                ..
            }
        )
    }

    #[test]
    fn frames_carry_their_placements_into_the_pane_view() {
        let mut view = RemoteView::default();
        let mut full = frame(1, 1, true, Vec::new());
        full.placements = vec![placement(4, -1), placement(5, 2)];
        view.apply_frame(full);
        assert_eq!(
            view.pane(1).unwrap().placements,
            vec![placement(4, -1), placement(5, 2)]
        );
        // Placements are always complete: a delta replaces them.
        view.apply_frame(frame(1, 2, false, Vec::new()));
        assert!(view.pane(1).unwrap().placements.is_empty());
    }

    #[test]
    fn images_larger_than_one_chunk_reassemble_exactly_through_the_encoder() {
        use crate::attach::{MAX_IMAGE_CHUNK_BYTES, image_announcement, image_data_messages};
        let (width, height) = (1100u32, 1000u32);
        let rgba: Vec<u8> = (0..(width * height * 4) as usize)
            .map(|index| (index * 7 % 256) as u8)
            .collect();
        assert!(rgba.len() > MAX_IMAGE_CHUNK_BYTES);
        let mut messages = vec![image_announcement(3, 9, width, height)];
        messages.extend(image_data_messages(3, 9, &rgba));
        assert_eq!(messages.len(), 3, "announcement plus two chunks");
        let mut view = RemoteView::default();
        let changes = deliver(&mut view, messages);
        assert_eq!(
            changes,
            vec![
                ViewChange::None,
                ViewChange::None,
                ViewChange::Image {
                    pane: 3,
                    image: 9,
                    change: ImageChange::Ready
                }
            ]
        );
        let image = view.image(3, 9).unwrap();
        assert_eq!((image.width, image.height), (width, height));
        assert!(image.rgba == rgba, "reassembled bytes differ");
        assert!(view.image(3, 8).is_none());
    }

    #[test]
    fn incomplete_images_are_not_available_for_rendering() {
        use crate::attach::image_announcement;
        let mut view = RemoteView::default();
        deliver(&mut view, vec![image_announcement(1, 1, 2, 1)]);
        assert!(view.image(1, 1).is_none());
        let change = view.apply(ServerMessage::ImageData {
            pane: 1,
            image: 1,
            offset: 0,
            data: vec![1, 2, 3, 4],
        });
        assert_eq!(change, ViewChange::None);
        assert!(view.image(1, 1).is_none(), "half of the bytes arrived");
        view.apply(ServerMessage::ImageData {
            pane: 1,
            image: 1,
            offset: 4,
            data: vec![5, 6, 7, 8],
        });
        assert_eq!(view.image(1, 1).unwrap().rgba, (1..=8).collect::<Vec<u8>>());
    }

    #[test]
    fn image_chunks_are_bounds_checked_against_the_announcement() {
        use crate::attach::image_announcement;
        let chunk = |offset: u64, data: Vec<u8>| ServerMessage::ImageData {
            pane: 1,
            image: 1,
            offset,
            data,
        };
        // Data without an announcement.
        let mut view = RemoteView::default();
        assert!(rejected(&view.apply(chunk(0, vec![1; 4]))));
        // A gap in the offsets drops the image; later chunks are ignored quietly.
        view.apply(image_announcement(1, 1, 2, 1));
        assert!(rejected(&view.apply(chunk(4, vec![1; 4]))));
        assert_eq!(view.apply(chunk(0, vec![1; 4])), ViewChange::None);
        assert!(view.image(1, 1).is_none());
        // More bytes than announced.
        view.apply(image_announcement(1, 1, 2, 1));
        assert!(rejected(&view.apply(chunk(0, vec![1; 12]))));
        assert!(view.image(1, 1).is_none());
        // A new announcement starts over and can complete.
        view.apply(image_announcement(1, 1, 1, 1));
        assert_eq!(
            view.apply(chunk(0, vec![9; 4])),
            ViewChange::Image {
                pane: 1,
                image: 1,
                change: ImageChange::Ready
            }
        );
        // Dimensions beyond the kitty limit are refused at announcement.
        assert!(rejected(&view.apply(image_announcement(1, 2, 10_001, 1))));
    }

    #[test]
    fn announced_image_bytes_are_capped_like_the_process_image_limit() {
        use crate::attach::image_announcement;
        let mut view = RemoteView::default();
        // 300 MB each: the first fits under 512 MiB, the second does not.
        assert_eq!(
            view.apply(image_announcement(1, 1, 10_000, 7_500)),
            ViewChange::None
        );
        assert!(rejected(
            &view.apply(image_announcement(1, 2, 10_000, 7_500))
        ));
        // Replacing an image releases its share first.
        assert_eq!(
            view.apply(image_announcement(1, 1, 10_000, 7_500)),
            ViewChange::None
        );
        view.apply(ServerMessage::ImageRemoved { pane: 1, image: 1 });
        assert_eq!(
            view.apply(image_announcement(1, 2, 10_000, 7_500)),
            ViewChange::None
        );
        assert_eq!(MAX_CLIENT_IMAGE_BYTES, 512 * 1024 * 1024);
    }

    #[test]
    fn removal_messages_and_vanished_panes_drop_their_images() {
        use crate::attach::image_announcement;
        let mut view = RemoteView::default();
        view.apply(ServerMessage::Layout(two_tab_layout(1)));
        for (pane, image) in [(1, 1), (2, 1), (2, 2)] {
            view.apply(image_announcement(pane, image, 1, 1));
            view.apply(ServerMessage::ImageData {
                pane,
                image,
                offset: 0,
                data: vec![0; 4],
            });
            assert!(view.image(pane, image).is_some());
        }
        assert_eq!(
            view.apply(ServerMessage::ImageRemoved { pane: 2, image: 2 }),
            ViewChange::Image {
                pane: 2,
                image: 2,
                change: ImageChange::Removed
            }
        );
        assert!(view.image(2, 2).is_none());
        // Removing an image the client never had is not a view change.
        assert_eq!(
            view.apply(ServerMessage::ImageRemoved { pane: 2, image: 2 }),
            ViewChange::None
        );
        let mut layout = two_tab_layout(2);
        layout.tabs.remove(0);
        layout.active_tab = 0;
        view.apply(ServerMessage::Layout(layout));
        assert!(view.image(1, 1).is_none(), "pane 1 is gone");
        assert!(view.image(2, 1).is_some());
    }

    #[test]
    fn reattach_order_resolves_placements_to_the_complete_image() {
        use crate::attach::{image_announcement, image_data_messages};
        // A fresh view after re-attach: layout, then image data, then the full frame.
        let mut view = RemoteView::default();
        let rgba: Vec<u8> = (0..48 * 48 * 4).map(|index| (index % 256) as u8).collect();
        let mut full = frame(2, 1, true, Vec::new());
        full.placements = vec![placement(3, 0)];
        let mut messages = vec![
            ServerMessage::Layout(two_tab_layout(1)),
            image_announcement(2, 3, 48, 48),
        ];
        messages.extend(image_data_messages(2, 3, &rgba));
        messages.push(ServerMessage::Frame(full));
        let changes = deliver(&mut view, messages);
        assert!(changes.contains(&ViewChange::Image {
            pane: 2,
            image: 3,
            change: ImageChange::Ready
        }));
        let pane = view.pane(2).unwrap();
        assert_eq!(pane.placements, vec![placement(3, 0)]);
        let image = view.image(2, pane.placements[0].image).unwrap();
        assert_eq!((image.width, image.height), (48, 48));
        assert!(image.rgba == rgba);
    }

    #[test]
    fn attach_waits_until_the_window_size_settles() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut settle = AttachSettle::new(start);
        assert_eq!(settle.deadline(), Some(start + ATTACH_SETTLE));
        assert!(!settle.due(start));
        // A window-manager resize restarts the wait, so `Hello` never carries the provisional size.
        let resized = start + ATTACH_SETTLE - Duration::from_millis(1);
        settle.resized(resized);
        assert!(!settle.due(start + ATTACH_SETTLE));
        assert!(settle.due(resized + ATTACH_SETTLE));
        settle.attached();
        assert_eq!(settle.deadline(), None);
        assert!(!settle.due(resized + ATTACH_SETTLE * 10));
        // Resizes after attaching are ordinary `Resize` messages, not new waits.
        settle.resized(resized + ATTACH_SETTLE * 11);
        assert_eq!(settle.deadline(), None);
        assert!(ATTACH_SETTLE <= Duration::from_millis(250));
    }

    #[test]
    fn window_metrics_build_hello_and_resize_messages() {
        let metrics = WindowMetrics::new(800, 600, 8.2, 17.1);
        assert_eq!(
            metrics,
            WindowMetrics {
                width: 800,
                height: 600,
                cell_width: 9,
                cell_height: 18
            }
        );
        assert_eq!(
            metrics.hello(),
            crate::attach::ClientMessage::Hello {
                width: 800,
                height: 600,
                cell_width: 9,
                cell_height: 18
            }
        );
        assert_eq!(
            metrics.resize(),
            crate::attach::ClientMessage::Resize {
                width: 800,
                height: 600,
                cell_width: 9,
                cell_height: 18
            }
        );
    }

    #[test]
    fn server_program_prefers_the_sibling_binary_then_path() {
        let dir = scratch_dir("program");
        let exe = dir.join("nulix");
        assert_eq!(
            server_program(Some(&exe)),
            std::path::PathBuf::from("nulix-server")
        );
        std::fs::write(dir.join("nulix-server"), b"").unwrap();
        assert_eq!(server_program(Some(&exe)), dir.join("nulix-server"));
        assert_eq!(
            server_program(None),
            std::path::PathBuf::from("nulix-server")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nulix-remote-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    mod transport {
        use super::scratch_dir;
        use crate::attach::{ClientMessage, ServerMessage, encode_server, parse_client};
        use crate::control::{
            ControlCommand, ControlOutcome, ControlResponse, parse_request, serialize_response,
        };
        use crate::remote::{AttachConnection, RemoteEvent, WindowMetrics, ensure_server_in};
        use serde_json::json;
        use std::{
            io::{BufRead, BufReader, Write},
            os::unix::net::{UnixListener, UnixStream},
            path::Path,
            sync::mpsc,
            thread,
            time::Duration,
        };

        fn read_line(reader: &mut BufReader<UnixStream>) -> Vec<u8> {
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).unwrap();
            line
        }

        /// In-process fake server speaking the documented attach upgrade.
        fn fake_server(socket: &Path, accept: bool) -> thread::JoinHandle<Vec<ClientMessage>> {
            let listener = UnixListener::bind(socket).unwrap();
            thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                let request = parse_request(&read_line(&mut reader)).unwrap();
                assert_eq!(request.command, ControlCommand::Attach);
                let outcome = if accept {
                    ControlOutcome::Success(json!({"attached": true, "session": "work"}))
                } else {
                    ControlOutcome::Failure(crate::control::ControlError::for_request(
                        Some(request.id),
                        crate::control::ControlErrorCode::OperationRefused,
                        "another client is attached to session work",
                    ))
                };
                let response = ControlResponse {
                    id: Some(request.id),
                    outcome,
                };
                // Success line and the first stream message in one write: the client
                // must not lose bytes buffered after the control reply.
                let mut bytes = serialize_response(&response).unwrap();
                if !accept {
                    writer.write_all(&bytes).unwrap();
                    return Vec::new();
                }
                bytes.extend(
                    encode_server(&ServerMessage::Welcome {
                        session: "work".into(),
                    })
                    .unwrap(),
                );
                writer.write_all(&bytes).unwrap();
                let mut received = Vec::new();
                loop {
                    let line = read_line(&mut reader);
                    if line.is_empty() {
                        break;
                    }
                    let message = parse_client(&line).unwrap();
                    let done = message == ClientMessage::Detach;
                    if let ClientMessage::Input { .. } = &message {
                        writer
                            .write_all(
                                &encode_server(&ServerMessage::Exit {
                                    reason: "shutdown".into(),
                                })
                                .unwrap(),
                            )
                            .unwrap();
                    }
                    received.push(message);
                    if done {
                        break;
                    }
                }
                received
            })
        }

        #[test]
        fn attach_upgrades_the_control_connection_and_relays_stream_messages() {
            let dir = scratch_dir("attach");
            let socket = dir.join("server.sock");
            let server = fake_server(&socket, true);
            let (sender, events) = mpsc::channel();
            let metrics = WindowMetrics::new(640, 480, 8.0, 16.0);
            let connection = AttachConnection::attach(&socket, metrics, move |event| {
                let _ = sender.send(event);
            })
            .unwrap();
            assert_eq!(connection.session(), "work");
            let wait = Duration::from_secs(5);
            assert_eq!(
                events.recv_timeout(wait).unwrap(),
                RemoteEvent::Message(ServerMessage::Welcome {
                    session: "work".into()
                })
            );
            connection
                .send(&ClientMessage::Input {
                    pane: 1,
                    data: b"ls\r".to_vec(),
                })
                .unwrap();
            assert_eq!(
                events.recv_timeout(wait).unwrap(),
                RemoteEvent::Message(ServerMessage::Exit {
                    reason: "shutdown".into()
                })
            );
            connection.detach().unwrap();
            let received = server.join().unwrap();
            assert_eq!(
                received,
                vec![
                    metrics.hello(),
                    ClientMessage::Input {
                        pane: 1,
                        data: b"ls\r".to_vec()
                    },
                    ClientMessage::Detach
                ]
            );
            assert_eq!(
                events.recv_timeout(wait).unwrap(),
                RemoteEvent::Closed(None)
            );
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn refused_attach_reports_the_server_message() {
            let dir = scratch_dir("refused");
            let socket = dir.join("server.sock");
            let server = fake_server(&socket, false);
            let error =
                AttachConnection::attach(&socket, WindowMetrics::new(1, 1, 1.0, 1.0), |_| {})
                    .err()
                    .expect("attach is refused");
            assert!(
                error
                    .to_string()
                    .contains("another client is attached to session work"),
                "{error}"
            );
            server.join().unwrap();
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn malformed_stream_line_closes_with_a_diagnostic() {
            let dir = scratch_dir("malformed");
            let socket = dir.join("server.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                let request = parse_request(&read_line(&mut reader)).unwrap();
                let response = ControlResponse {
                    id: Some(request.id),
                    outcome: ControlOutcome::Success(json!({"attached": true, "session": "s"})),
                };
                writer
                    .write_all(&serialize_response(&response).unwrap())
                    .unwrap();
                read_line(&mut reader);
                writer.write_all(b"{\"v\":1,\"type\":\"nope\"}\n").unwrap();
                thread::sleep(Duration::from_millis(200));
            });
            let (sender, events) = mpsc::channel();
            let _connection =
                AttachConnection::attach(&socket, WindowMetrics::new(1, 1, 1.0, 1.0), move |e| {
                    let _ = sender.send(e);
                })
                .unwrap();
            match events.recv_timeout(Duration::from_secs(5)).unwrap() {
                RemoteEvent::Closed(Some(message)) => {
                    assert!(message.contains("invalid attach message"), "{message}")
                }
                other => panic!("unexpected event {other:?}"),
            }
            server.join().unwrap();
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn ensure_server_reuses_a_live_socket_without_spawning() {
            let dir = scratch_dir("live");
            let socket = dir.join(crate::control::session_socket_name("live"));
            let _listener = UnixListener::bind(&socket).unwrap();
            let path = ensure_server_in(
                &dir,
                "live",
                Path::new("/nonexistent/nulix-server"),
                Duration::from_millis(100),
            )
            .unwrap();
            assert_eq!(path, socket);
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn ensure_server_reports_a_server_that_exits_before_listening() {
            let dir = scratch_dir("exits");
            let error = ensure_server_in(
                &dir,
                "gone",
                Path::new("/bin/false"),
                Duration::from_secs(5),
            )
            .unwrap_err();
            assert!(error.to_string().contains("exited before"), "{error}");
            let error = ensure_server_in(
                &dir,
                "missing",
                Path::new("/nonexistent/nulix-server"),
                Duration::from_secs(5),
            )
            .unwrap_err();
            assert!(error.to_string().contains("could not start"), "{error}");
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn ensure_server_rejects_invalid_session_names() {
            let dir = scratch_dir("invalid");
            let error = ensure_server_in(
                &dir,
                "bad name",
                Path::new("/bin/false"),
                Duration::from_millis(100),
            )
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

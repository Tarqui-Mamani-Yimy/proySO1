//! Headless terminal session owned by the persistent `so1term-server` (Phase 5 T10).
//!
//! A [`Session`] owns the tab/pane layout and, for every pane, the shell PTY and
//! its VT [`Model`]. It has no window, GPU, or event-loop dependency: PTY output
//! is announced through a caller-supplied [`OutputNotifier`], and control
//! requests are answered by [`Session::handle_control`]. Panes keep running while
//! no client is connected, which is what makes detach possible.
//!
//! Panes use a virtual grid of [`VIRTUAL_CELL_WIDTH`] x [`VIRTUAL_CELL_HEIGHT`]
//! pixel cells until an attached client provides real geometry with
//! [`Session::set_client_geometry`]. From then on the client's window and cell
//! metrics stay authoritative (also after it detaches), and pane rects are laid
//! out below a one-cell-high tab bar whenever more than one tab exists, the same
//! rule as the window's `tab_content_rect`.
//!
//! Kitty graphics (Phase 5 T11d): every pane runs the shared
//! [`crate::graphics_pipeline`], exactly like a standalone window pane. Replies
//! go to the pane PTY, `SO1TERM_GRAPHICS_ALLOWED_ROOTS` guards file media in
//! this process, and decoding runs on a worker thread whose result comes back
//! through the [`DecodeNotifier`] and [`Session::finish_graphics`]. The
//! session records which images were stored or freed ([`ImageEvent`]) so the
//! server can announce image data to an attached client before frames place
//! it, and frames carry the projected placements.
//!
//! Attach support (Phase 5 T11b) is split into pure pieces the server loop
//! combines: [`Session::handle_client`] runs client view and layout actions on
//! the models, [`AttachFrames`] turns model state into layout revisions and
//! per-pane full or delta [`PaneFrame`]s, and [`Outbox`] is the bounded
//! outgoing queue between the server loop and a client's writer thread.
//!
//! Threading model: a `Session` is owned by exactly one thread (the
//! `so1term-server` loop) and is never shared. PTY reader threads only call the
//! [`OutputNotifier`]; control and attach threads talk to the loop through its
//! event channel. The only state shared with another thread is the [`Outbox`],
//! which the caller wraps in a mutex: the loop pushes without ever blocking on
//! the client, and the writer thread pops and performs the socket writes.
//!
//! The title and tab helpers here are also used by the `so1term` window so both
//! frontends name panes and roll back failed tabs the same way.

use crate::attach::{
    self, BlockOp, ClientMessage, CopySource, FramePlacement, LayoutAction, LayoutFrame,
    LayoutPane, LayoutTab, PaneFrame, PaneModes, SelectionSpan, ServerMessage, SplitDirection,
    StyledRun,
};
use crate::blocks::GridAnchor;
use crate::control::{
    ControlCommand, ControlDirection, ControlError, ControlErrorCode, ControlOutcome,
    ControlRequest, ControlResponse, ListPane, TextScope, list_pane_entry,
};
use crate::graphics::{self, DecodeJob, DecodedImage, GraphicsCompletion};
use crate::graphics_pipeline::{GraphicsHost, PaneGraphics, SharedGraphics, frame_placements};
use crate::layout::{
    CellSize, Direction, FocusDirection, PaneCloseResult, PaneId, PaneLayout, Rect, Tab,
    TabCloseResult, TabSet,
};
use crate::model::{Cursor, Model, ModelEvent};
use crate::pty::Pty;
use crate::renderer::WakeupGate;
use crate::renderer::{BlockDecoration, BlockViewport, project_block_decorations};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};
use vte::ansi::CursorShape;

/// Width in pixels of one virtual server cell.
pub const VIRTUAL_CELL_WIDTH: u32 = 8;
/// Height in pixels of one virtual server cell.
pub const VIRTUAL_CELL_HEIGHT: u32 = 16;
/// Initial server grid columns.
pub const DEFAULT_COLS: u32 = 80;
/// Initial server grid rows.
pub const DEFAULT_ROWS: u32 = 24;

/// Largest client window an attached client may request, in cells per row.
pub const MAX_CLIENT_COLS: u32 = 1000;
/// Largest client window an attached client may request, in cell rows.
pub const MAX_CLIENT_ROWS: u32 = 500;

// Same layout limits as the so1term window.
const DIVIDER_WIDTH: u32 = 4;
const MIN_PANE_COLS: u32 = 20;
const MIN_PANE_ROWS: u32 = 5;

/// Called from a PTY reader thread when a pane has new output to receive.
pub type OutputNotifier = Arc<dyn Fn(PaneId) + Send + Sync>;

/// Called from a decode worker thread with a finished kitty image decode; the
/// owner of the session passes it to [`Session::finish_graphics`].
pub type DecodeNotifier = Arc<dyn Fn(PaneId, GridAnchor, GraphicsCompletion) + Send + Sync>;

/// Image data change recorded for attached clients.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageEvent {
    /// New or replaced data for the image.
    Stored(PaneId, u32),
    /// The image data was freed (delete with data, or quota eviction).
    Freed(PaneId, u32),
}

/// The server side of the graphics pipeline: no GPU, so every image is
/// displayable; changes are recorded for the attached client.
struct SessionGraphicsHost<'a> {
    cell: (f32, f32),
    decode: &'a DecodeNotifier,
    events: &'a mut Vec<ImageEvent>,
}

impl GraphicsHost for SessionGraphicsHost<'_> {
    fn cell_size(&self) -> (f32, f32) {
        self.cell
    }

    fn image_stored(&mut self, pane: PaneId, id: u32, _image: &DecodedImage) -> bool {
        self.events.push(ImageEvent::Stored(pane, id));
        true
    }

    fn image_displayable(&mut self, _pane: PaneId, _id: u32, _image: &DecodedImage) -> bool {
        true
    }

    fn image_freed(&mut self, pane: PaneId, id: u32) {
        self.events.push(ImageEvent::Freed(pane, id));
    }

    fn start_decode(&mut self, pane: PaneId, anchor: GridAnchor, job: DecodeJob) {
        let decode = Arc::clone(self.decode);
        std::thread::spawn(move || decode(pane, anchor, graphics::decode_transfer(job)));
    }
}

/// What the server should do after a control request was answered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlEffect {
    Continue,
    Shutdown,
}

struct SessionPane {
    pty: Pty,
    model: Model,
    title: String,
    explicit_title: Option<String>,
    cwd: PathBuf,
    /// True once the shell reported its directory with OSC 7.
    cwd_reported: bool,
    wakeup: Arc<WakeupGate>,
    graphics: PaneGraphics,
}

impl SessionPane {
    /// Applies queued PTY segments and answers terminal queries.
    fn apply(&mut self, id: PaneId, shared: &mut SharedGraphics, host: &mut dyn GraphicsHost) {
        let Self {
            pty,
            model,
            graphics,
            title,
            explicit_title,
            cwd,
            cwd_reported,
            ..
        } = self;
        graphics.apply(
            id,
            model,
            shared,
            host,
            &mut |reply| {
                let _ = pty.write(reply);
            },
            &mut |event| {
                if let crate::osc::Event::CurrentDirectory { path, .. } = event {
                    *cwd = PathBuf::from(String::from_utf8_lossy(&path).into_owned());
                    *cwd_reported = true;
                    *title = pane_tab_title(cwd, explicit_title.as_deref());
                }
            },
        );
        for event in model.take_events() {
            if let Some(new_title) = apply_title_event(cwd, explicit_title, &event) {
                *title = new_title;
                continue;
            }
            if let ModelEvent::PtyWrite(bytes) = event {
                let _ = pty.write(&bytes);
            }
        }
    }
}

pub struct Session {
    tabs: TabSet,
    window: Rect,
    cell: CellSize,
    /// True once an attached client supplied the window and cell metrics.
    client_geometry: bool,
    panes: BTreeMap<PaneId, SessionPane>,
    start_cwd: PathBuf,
    notify: OutputNotifier,
    decode: DecodeNotifier,
    graphics: SharedGraphics,
    image_events: Vec<ImageEvent>,
}

impl Session {
    /// Creates the session with one tab running one shell in `cwd`.
    pub fn start(
        cols: u32,
        rows: u32,
        cwd: PathBuf,
        notify: OutputNotifier,
        decode: DecodeNotifier,
    ) -> Result<Self, String> {
        let cell = CellSize::new(VIRTUAL_CELL_WIDTH, VIRTUAL_CELL_HEIGHT);
        let window = Rect::new(
            0,
            0,
            cols.max(MIN_PANE_COLS) * VIRTUAL_CELL_WIDTH,
            rows.max(MIN_PANE_ROWS) * VIRTUAL_CELL_HEIGHT,
        );
        let tabs = TabSet::new(window, cell, DIVIDER_WIDTH, MIN_PANE_COLS, MIN_PANE_ROWS);
        let tab = tabs.active().ok_or("initial tab was not created")?;
        let pane_id = tab.active_pane();
        let layout = visible_layout(tab, pane_id).ok_or("initial pane is not visible")?;
        let pane = spawn_pane(pane_id, &layout, cwd.clone(), &notify)?;
        Ok(Self {
            tabs,
            window,
            cell,
            client_geometry: false,
            panes: BTreeMap::from([(pane_id, pane)]),
            start_cwd: cwd,
            notify,
            decode,
            graphics: SharedGraphics::default(),
            image_events: Vec::new(),
        })
    }

    /// True once every pane has closed; the server then exits.
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Process ids of the pane shells, in pane order.
    pub fn shell_pids(&self) -> Vec<u32> {
        self.panes
            .values()
            .filter_map(|pane| pane.pty.process_id())
            .collect()
    }

    /// Feeds pending PTY output of `pane_id` into its model and answers terminal queries.
    pub fn receive_output(&mut self, pane_id: PaneId) {
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        // Acknowledge before draining so output that arrives meanwhile notifies again.
        pane.wakeup.acknowledge();
        for bytes in pane.pty.drain_output() {
            pane.graphics.receive(&bytes);
        }
        let mut host = SessionGraphicsHost {
            cell: cell_pixels(self.cell),
            decode: &self.decode,
            events: &mut self.image_events,
        };
        pane.apply(pane_id, &mut self.graphics, &mut host);
    }

    /// Completes a kitty image decode reported through the [`DecodeNotifier`]
    /// and resumes the pane output that waited for it.
    pub fn finish_graphics(
        &mut self,
        pane_id: PaneId,
        anchor: GridAnchor,
        completion: GraphicsCompletion,
    ) {
        let mut host = SessionGraphicsHost {
            cell: cell_pixels(self.cell),
            decode: &self.decode,
            events: &mut self.image_events,
        };
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        let SessionPane {
            pty,
            model,
            graphics,
            ..
        } = pane;
        graphics.finish_decode(
            pane_id,
            anchor,
            completion,
            model,
            &mut self.graphics,
            &mut host,
            &mut |reply| {
                let _ = pty.write(reply);
            },
        );
        for (evicted_pane, image) in self.graphics.take_evicted(&mut host) {
            if let Some(pane) = self.panes.get_mut(&evicted_pane) {
                pane.graphics
                    .forget_evicted(evicted_pane, image, &self.graphics.store);
            }
        }
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.apply(pane_id, &mut self.graphics, &mut host);
        }
    }

    /// Stored RGBA data of a pane's kitty image.
    pub fn image(&self, pane: PaneId, id: u32) -> Option<&DecodedImage> {
        self.graphics.store.get(pane, id)
    }

    /// Image data changes since the last call, in order.
    pub fn take_image_events(&mut self) -> Vec<ImageEvent> {
        std::mem::take(&mut self.image_events)
    }

    /// Closes every pane whose shell has exited. Returns how many closed.
    pub fn reap_exited(&mut self) -> usize {
        let exited: Vec<_> = self
            .panes
            .iter_mut()
            .filter_map(|(id, pane)| pane.pty.try_wait().ok().flatten().map(|_| *id))
            .collect();
        for pane in &exited {
            self.close(*pane);
        }
        exited.len()
    }

    /// Answers one control request.
    pub fn handle_control(&mut self, request: ControlRequest) -> (ControlResponse, ControlEffect) {
        let id = Some(request.id);
        let fail = |code, message: String| {
            (
                ControlResponse {
                    id,
                    outcome: ControlOutcome::Failure(ControlError::for_request(id, code, message)),
                },
                ControlEffect::Continue,
            )
        };
        let mut effect = ControlEffect::Continue;
        let result = match request.command {
            ControlCommand::List => self.list(),
            ControlCommand::NewTab => match self.new_tab() {
                Ok(pane) => serde_json::json!({
                    "index": self.tabs.active_index().unwrap_or(0),
                    "pane": pane.0,
                }),
                Err(error) => return fail(ControlErrorCode::OperationRefused, error),
            },
            ControlCommand::Split { pane, direction } => {
                if let Some(pane_id) = pane
                    && !self.focus(PaneId(pane_id))
                {
                    return fail(
                        ControlErrorCode::NotFound,
                        format!("pane {pane_id} does not exist"),
                    );
                }
                let direction = match direction {
                    ControlDirection::Right => Direction::Horizontal,
                    ControlDirection::Down => Direction::Vertical,
                };
                match self.split_focused(direction) {
                    Ok(pane) => serde_json::json!({"pane": pane.0}),
                    Err(error) => return fail(ControlErrorCode::OperationRefused, error),
                }
            }
            ControlCommand::Focus { pane } => {
                if !self.focus(PaneId(pane)) {
                    return fail(
                        ControlErrorCode::NotFound,
                        format!("pane {pane} does not exist"),
                    );
                }
                serde_json::json!({"pane": pane})
            }
            ControlCommand::Close { pane } => {
                if self.tabs.tab_index_for_pane(PaneId(pane)).is_none() {
                    return fail(
                        ControlErrorCode::NotFound,
                        format!("pane {pane} does not exist"),
                    );
                }
                self.close(PaneId(pane));
                serde_json::json!({"closed": pane})
            }
            ControlCommand::SendText { pane, text } => {
                let Some(pane_id) = pane.map(PaneId).or_else(|| self.focused_pane_id()) else {
                    return fail(ControlErrorCode::Unavailable, "no focused pane".into());
                };
                let Some(target) = self.panes.get_mut(&pane_id) else {
                    return fail(
                        ControlErrorCode::NotFound,
                        format!("pane {} does not exist", pane_id.0),
                    );
                };
                if let Err(error) = target.pty.write(text.as_bytes()) {
                    return fail(
                        ControlErrorCode::Unavailable,
                        format!("could not write to PTY: {error}"),
                    );
                }
                target.model.scroll_to_bottom();
                serde_json::json!({"pane": pane_id.0, "written": text.len()})
            }
            ControlCommand::Block { pane, block } => {
                let Some(pane_id) = pane.map(PaneId).or_else(|| self.focused_pane_id()) else {
                    return fail(ControlErrorCode::Unavailable, "no focused pane".into());
                };
                let Some(target) = self.panes.get(&pane_id) else {
                    return fail(
                        ControlErrorCode::NotFound,
                        format!("pane {} does not exist", pane_id.0),
                    );
                };
                match crate::control::block_result(pane_id.0, &target.model, block) {
                    Ok(result) => result,
                    Err((code, message)) => return fail(code, message),
                }
            }
            ControlCommand::GetText { pane, scope } => {
                let Some(pane_id) = pane.map(PaneId).or_else(|| self.focused_pane_id()) else {
                    return fail(ControlErrorCode::Unavailable, "no focused pane".into());
                };
                let Some(target) = self.panes.get(&pane_id) else {
                    return fail(
                        ControlErrorCode::NotFound,
                        format!("pane {} does not exist", pane_id.0),
                    );
                };
                let text = match scope {
                    TextScope::Screen => Some(target.model.visible_text()),
                    TextScope::LastBlock => target.model.last_finished_block_output_text(),
                };
                let Some(text) = text else {
                    return fail(
                        ControlErrorCode::OperationRefused,
                        "last finished block is unavailable or its anchors were evicted".into(),
                    );
                };
                serde_json::json!({"pane": pane_id.0, "text": text})
            }
            ControlCommand::Shutdown => {
                effect = ControlEffect::Shutdown;
                serde_json::json!({"stopping": true})
            }
            // so1term-server intercepts attach in its listener hook; reaching the
            // dispatcher means the listener has no attach support.
            ControlCommand::Attach => {
                return fail(
                    ControlErrorCode::OperationRefused,
                    "attach is not supported on this listener".into(),
                );
            }
        };
        (
            ControlResponse {
                id,
                outcome: ControlOutcome::Success(result),
            },
            effect,
        )
    }

    fn list(&self) -> serde_json::Value {
        let active = self.tabs.active_index().unwrap_or(0);
        let tabs: Vec<_> = self
            .tabs
            .tabs()
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let panes: Vec<_> = tab
                    .layout_tree()
                    .panes()
                    .iter()
                    .map(|pane_id| {
                        let pane = self.panes.get(pane_id).map(|pane| ListPane {
                            cwd: &pane.cwd,
                            title: &pane.title,
                            model: &pane.model,
                        });
                        let focused = index == active && tab.active_pane() == *pane_id;
                        list_pane_entry(pane_id.0, focused, pane)
                    })
                    .collect();
                serde_json::json!({"index": index, "active": index == active, "panes": panes})
            })
            .collect();
        serde_json::json!({"tabs": tabs})
    }

    fn focused_pane_id(&self) -> Option<PaneId> {
        self.tabs.active().map(Tab::active_pane)
    }

    /// New panes start in the focused pane's last reported directory.
    fn focused_cwd(&self) -> PathBuf {
        self.focused_pane_id()
            .and_then(|pane| self.panes.get(&pane))
            .map_or_else(|| self.start_cwd.clone(), |pane| pane.cwd.clone())
    }

    fn focus(&mut self, pane: PaneId) -> bool {
        let Some(index) = self.tabs.tab_index_for_pane(pane) else {
            return false;
        };
        self.tabs.select(index);
        self.tabs
            .tab_mut(index)
            .is_some_and(|tab| tab.focus_pane(pane))
    }

    fn new_tab(&mut self) -> Result<PaneId, String> {
        let cwd = self.focused_cwd();
        let notify = Arc::clone(&self.notify);
        let (pane_id, pane) = create_tab_with(&mut self.tabs, |pane_id, layout| {
            spawn_pane(pane_id, &layout, cwd, &notify)
        })?;
        self.panes.insert(pane_id, pane);
        // A second tab can add the tab bar, which shrinks every pane.
        self.apply_layout();
        Ok(pane_id)
    }

    /// Closes the active tab and its panes; the session ends with the last tab.
    fn close_active_tab(&mut self) -> Result<(), String> {
        let panes = self
            .tabs
            .active()
            .ok_or_else(|| "no active tab".to_owned())?
            .layout_tree()
            .panes();
        match self.tabs.close_active() {
            TabCloseResult::LastTab => self.panes.clear(),
            TabCloseResult::Closed(_) => {
                for pane in panes {
                    self.panes.remove(&pane);
                    self.graphics.store.remove_pane(pane);
                }
                self.apply_layout();
            }
        }
        Ok(())
    }

    fn split_focused(&mut self, direction: Direction) -> Result<PaneId, String> {
        let cwd = self.focused_cwd();
        let tab = self
            .tabs
            .active_mut()
            .ok_or_else(|| "pane layout is not initialized".to_owned())?;
        let pane_id = tab
            .split_focused(direction)
            .map_err(|error| format!("cannot split pane: {error:?}"))?;
        let created = visible_layout(tab, pane_id)
            .ok_or_else(|| "new pane does not satisfy the minimum size".to_owned())
            .and_then(|layout| spawn_pane(pane_id, &layout, cwd, &self.notify));
        match created {
            Ok(pane) => {
                self.panes.insert(pane_id, pane);
                self.apply_layout();
                Ok(pane_id)
            }
            Err(error) => {
                if let Some(tab) = self.tabs.active_mut() {
                    let _ = tab.close_pane(pane_id);
                }
                Err(error)
            }
        }
    }

    /// Removes `pane` from the layout and stops its shell.
    fn close(&mut self, pane: PaneId) {
        let last = close_pane_lifecycle(&mut self.tabs, pane);
        self.panes.remove(&pane);
        self.graphics.store.remove_pane(pane);
        if last {
            self.panes.clear();
        } else {
            self.apply_layout();
        }
    }

    /// Pane area: the window, minus a one-cell tab bar for client geometry with several tabs.
    fn content_rect(&self) -> Rect {
        if self.client_geometry {
            tab_content_rect(self.window, self.tabs.tabs().len(), self.cell.height)
        } else {
            self.window
        }
    }

    /// Resizes visible pane models and PTYs to the current layout.
    fn apply_layout(&mut self) {
        self.tabs.resize_all(self.content_rect(), self.cell);
        let layouts: Vec<_> = self.tabs.tabs().iter().flat_map(Tab::layout).collect();
        for layout in layouts.into_iter().filter(|layout| !layout.hidden) {
            if let Some(pane) = self.panes.get_mut(&layout.pane) {
                pane.graphics.resize_model(
                    &mut pane.model,
                    layout.cols as usize,
                    layout.rows as usize,
                );
                let _ = pane.pty.resize(
                    layout.cols.min(u16::MAX as u32) as u16,
                    layout.rows.min(u16::MAX as u32) as u16,
                    layout.rect.width.min(u16::MAX as u32) as u16,
                    layout.rect.height.min(u16::MAX as u32) as u16,
                );
            }
        }
    }
}

/// Everything a [`PaneFrame`] reports about one pane, computed from its model.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneSnapshot {
    pub cols: usize,
    pub rows: Vec<Vec<StyledRun>>,
    pub meta: FrameMeta,
}

/// Frame fields that are always sent complete, compared to detect changes.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameMeta {
    pub cursor: Cursor,
    pub selection: Vec<SelectionSpan>,
    pub modes: PaneModes,
    pub decorations: Vec<BlockDecoration>,
    pub title: String,
    pub cwd: String,
    pub placements: Vec<FramePlacement>,
}

/// Attach error code for unknown pane ids.
pub const ERROR_NOT_FOUND: &str = "NOT_FOUND";
/// Attach error code for a refused or failed action.
pub const ERROR_UNAVAILABLE: &str = "UNAVAILABLE";
/// Attach error code for invalid client window metrics.
pub const ERROR_INVALID_GEOMETRY: &str = "INVALID_GEOMETRY";
/// Attach error code sent before detaching a client whose queued image data
/// still exceeds [`OUTBOX_MAX_IMAGE_BYTES`] when another image must start.
pub const ERROR_IMAGE_BACKLOG: &str = "IMAGE_BACKLOG";

/// Builds a [`ServerMessage::Error`].
pub fn attach_error(code: &str, message: impl Into<String>) -> ServerMessage {
    ServerMessage::Error {
        code: code.to_owned(),
        message: message.into(),
    }
}

impl Session {
    /// Adopts an attached client's window size and cell metrics (pixels) and
    /// resizes every visible pane model and PTY. Metrics that would produce
    /// an empty cell or a grid beyond [`MAX_CLIENT_COLS`] x [`MAX_CLIENT_ROWS`]
    /// are refused and leave the geometry unchanged.
    pub fn set_client_geometry(
        &mut self,
        width: u32,
        height: u32,
        cell_width: u32,
        cell_height: u32,
    ) -> Result<(), String> {
        if cell_width == 0 || cell_height == 0 {
            return Err("cell size must be at least 1x1 pixels".into());
        }
        if width / cell_width > MAX_CLIENT_COLS || height / cell_height > MAX_CLIENT_ROWS {
            return Err(format!(
                "window exceeds {MAX_CLIENT_COLS}x{MAX_CLIENT_ROWS} cells"
            ));
        }
        self.window = Rect::new(0, 0, width, height);
        self.cell = CellSize::new(cell_width, cell_height);
        self.client_geometry = true;
        self.apply_layout();
        Ok(())
    }

    /// True when `pane` exists in the session.
    pub fn has_pane(&self, pane: PaneId) -> bool {
        self.panes.contains_key(&pane)
    }

    /// Visible panes of the active tab, in layout order.
    pub fn visible_panes(&self) -> Vec<PaneId> {
        self.tabs
            .active()
            .map(|tab| {
                tab.layout()
                    .into_iter()
                    .filter(|layout| !layout.hidden)
                    .map(|layout| layout.pane)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Active tab index and every tab's geometry; pane rects are window pixels.
    pub fn layout_tabs(&self) -> (usize, Vec<LayoutTab>) {
        let tabs = self
            .tabs
            .tabs()
            .iter()
            .map(|tab| {
                let focused = tab.active_pane();
                LayoutTab {
                    title: self
                        .panes
                        .get(&focused)
                        .map(|pane| pane.title.clone())
                        .unwrap_or_default(),
                    panes: tab
                        .layout()
                        .into_iter()
                        .map(|layout| LayoutPane {
                            pane: layout.pane.0,
                            x: layout.rect.x,
                            y: layout.rect.y,
                            width: layout.rect.width,
                            height: layout.rect.height,
                            cols: layout.cols as usize,
                            rows: layout.rows as usize,
                            hidden: layout.hidden,
                            focused: layout.pane == focused,
                        })
                        .collect(),
                }
            })
            .collect();
        (self.tabs.active_index().unwrap_or(0), tabs)
    }

    /// Projects one pane's model the same way the window renders it.
    pub fn pane_snapshot(&self, pane: PaneId) -> Option<PaneSnapshot> {
        let pane = self.panes.get(&pane)?;
        let model = &pane.model;
        let cells = model.visible_cells();
        let (cols, rows) = model.grid_dimensions();
        let cursor = model.cursor();
        // The model reports a cursor outside the viewport (scrolled history) as row usize::MAX.
        let cursor = if cursor.row >= rows || cursor.column >= cols {
            Cursor {
                column: 0,
                row: 0,
                shape: CursorShape::Hidden,
            }
        } else {
            cursor
        };
        let viewport = BlockViewport {
            evicted_rows: model.block_row_base(),
            history_size: model.history_size(),
            display_offset: model.display_offset(),
            first_visible_line: Some(model.visible_top_line()),
            screen_lines: cells.len(),
            alternate_screen: model.alternate_screen_active(),
        };
        Some(PaneSnapshot {
            cols,
            rows: cells.iter().map(|row| attach::line_runs(row)).collect(),
            meta: FrameMeta {
                cursor,
                selection: attach::selection_spans(&model.selected_cells()),
                modes: PaneModes {
                    keyboard_flags: model.keyboard_flags().bits(),
                    app_cursor: model.application_cursor_keys(),
                    bracketed_paste: model.bracketed_paste_enabled(),
                    mouse_or_alternate: model.mouse_or_alternate_screen_active(),
                    alternate_screen: model.alternate_screen_active(),
                },
                decorations: project_block_decorations(model.blocks(), viewport),
                title: pane.title.clone(),
                cwd: if pane.cwd_reported {
                    pane.cwd.to_string_lossy().into_owned()
                } else {
                    String::new()
                },
                placements: frame_placements(&pane.graphics.placements, model),
            },
        })
    }

    /// Runs one attached-client view or layout action and returns its reply.
    ///
    /// `Hello`, `Resync` and `Detach` concern the stream itself and are handled
    /// by the server loop; they return `None` here. Unknown pane ids return an
    /// `Error` with code [`ERROR_NOT_FOUND`].
    pub fn handle_client(&mut self, message: ClientMessage) -> Option<ServerMessage> {
        let missing =
            |pane: u64| attach_error(ERROR_NOT_FOUND, format!("pane {pane} does not exist"));
        match message {
            ClientMessage::Hello { .. } | ClientMessage::Resync { .. } | ClientMessage::Detach => {
                None
            }
            ClientMessage::Resize {
                width,
                height,
                cell_width,
                cell_height,
            } => self
                .set_client_geometry(width, height, cell_width, cell_height)
                .err()
                .map(|error| attach_error(ERROR_INVALID_GEOMETRY, error)),
            ClientMessage::Input { pane, data } => {
                let Some(target) = self.panes.get_mut(&PaneId(pane)) else {
                    return Some(missing(pane));
                };
                if let Err(error) = target.pty.write(&data) {
                    return Some(attach_error(
                        ERROR_UNAVAILABLE,
                        format!("could not write to PTY: {error}"),
                    ));
                }
                target.model.scroll_to_bottom();
                None
            }
            ClientMessage::Scroll { pane, lines } => {
                let Some(target) = self.panes.get_mut(&PaneId(pane)) else {
                    return Some(missing(pane));
                };
                if lines >= 0 {
                    target.model.scroll_up(lines as usize);
                } else {
                    target.model.scroll_down(lines.unsigned_abs() as usize);
                }
                None
            }
            ClientMessage::ScrollToBottom { pane } => {
                let Some(target) = self.panes.get_mut(&PaneId(pane)) else {
                    return Some(missing(pane));
                };
                target.model.scroll_to_bottom();
                None
            }
            ClientMessage::Selection { pane, op } => {
                let Some(target) = self.panes.get_mut(&PaneId(pane)) else {
                    return Some(missing(pane));
                };
                let (cols, rows) = target.model.grid_dimensions();
                // Clamp so client coordinates can never index outside the grid.
                let clamp = |column: usize, row: usize| {
                    (
                        column.min(cols.saturating_sub(1)),
                        row.min(rows.saturating_sub(1)),
                    )
                };
                match op {
                    attach::SelectionOp::Begin { column, row, kind } => {
                        let (column, row) = clamp(column, row);
                        target.model.begin_selection(column, row, kind);
                    }
                    attach::SelectionOp::Extend { column, row } => {
                        let (column, row) = clamp(column, row);
                        target.model.extend_selection(column, row);
                    }
                    attach::SelectionOp::Clear => target.model.clear_selection(),
                }
                None
            }
            ClientMessage::Block { pane, op } => {
                let Some(target) = self.panes.get_mut(&PaneId(pane)) else {
                    return Some(missing(pane));
                };
                use crate::block_navigation::NavigationDirection;
                match op {
                    BlockOp::Previous => target.model.navigate_block(NavigationDirection::Previous),
                    BlockOp::Next => target.model.navigate_block(NavigationDirection::Next),
                    BlockOp::Select => {
                        target.model.select_block_at_viewport();
                    }
                }
                None
            }
            ClientMessage::Copy {
                request,
                pane,
                source,
            } => {
                let Some(target) = self.panes.get(&PaneId(pane)) else {
                    return Some(missing(pane));
                };
                let text = match source {
                    CopySource::Selection => target.model.selection_text(),
                    CopySource::BlockInput => target.model.selected_block_input_text(),
                    CopySource::BlockOutput => target.model.selected_block_output_text(),
                };
                Some(ServerMessage::CopyText { request, text })
            }
            ClientMessage::Layout { request, action } => Some(ServerMessage::LayoutResult {
                request,
                error: self.apply_layout_action(action).err(),
            }),
        }
    }

    /// Performs a client tab/pane action with the same helpers as control requests.
    fn apply_layout_action(&mut self, action: LayoutAction) -> Result<(), String> {
        match action {
            LayoutAction::NewTab => self.new_tab().map(drop),
            LayoutAction::CloseTab => self.close_active_tab(),
            LayoutAction::NextTab => {
                self.tabs.next();
                Ok(())
            }
            LayoutAction::PreviousTab => {
                self.tabs.prev();
                Ok(())
            }
            LayoutAction::SelectTab { index } => {
                if self.tabs.select(index) {
                    Ok(())
                } else {
                    Err(format!("tab {index} does not exist"))
                }
            }
            LayoutAction::Split { direction } => self
                .split_focused(match direction {
                    SplitDirection::Right => Direction::Horizontal,
                    SplitDirection::Down => Direction::Vertical,
                })
                .map(drop),
            LayoutAction::ClosePane => {
                let pane = self
                    .focused_pane_id()
                    .ok_or_else(|| "no focused pane".to_owned())?;
                self.close(pane);
                Ok(())
            }
            LayoutAction::FocusDirection { direction } => {
                let direction = match direction {
                    attach::FocusDirection::Left => FocusDirection::Left,
                    attach::FocusDirection::Right => FocusDirection::Right,
                    attach::FocusDirection::Up => FocusDirection::Up,
                    attach::FocusDirection::Down => FocusDirection::Down,
                };
                if let Some(tab) = self.tabs.active_mut() {
                    tab.move_focus(direction);
                }
                Ok(())
            }
            LayoutAction::FocusPane { pane } => {
                if self.focus(PaneId(pane)) {
                    Ok(())
                } else {
                    Err(format!("pane {pane} does not exist"))
                }
            }
        }
    }
}

/// Pixel cell size the graphics pipeline sizes placements with.
fn cell_pixels(cell: CellSize) -> (f32, f32) {
    (cell.width as f32, cell.height as f32)
}

/// Window area left for panes below the tab bar, as in the so1term window:
/// a bar of one cell height is reserved only while more than one tab exists.
pub fn tab_content_rect(window: Rect, tab_count: usize, cell_height: u32) -> Rect {
    let bar = if tab_count <= 1 {
        0
    } else {
        cell_height.max(1)
    };
    let bar = bar.min(window.height);
    Rect::new(
        window.x,
        window.y.saturating_add(bar),
        window.width,
        window.height.saturating_sub(bar),
    )
}

#[derive(Debug)]
struct PaneTrack {
    seq: u64,
    cols: usize,
    rows: Vec<Vec<StyledRun>>,
    meta: Option<FrameMeta>,
    needs_full: bool,
}

impl Default for PaneTrack {
    fn default() -> Self {
        Self {
            seq: 0,
            cols: 0,
            rows: Vec::new(),
            meta: None,
            needs_full: true,
        }
    }
}

/// Per-client frame state: the last layout and per-pane rows sent to one client.
///
/// A new attach starts a new `AttachFrames`, so pane sequence numbers restart
/// at 1 for every client. Frames are only produced for visible panes of the
/// active tab; a pane that becomes visible (tab switch, split, resize) gets a
/// full frame, and so does every pane marked with [`Self::mark_full`].
#[derive(Debug, Default)]
pub struct AttachFrames {
    panes: BTreeMap<PaneId, PaneTrack>,
    visible: Vec<PaneId>,
    layout: Option<(usize, Vec<LayoutTab>)>,
    revision: u64,
}

impl AttachFrames {
    /// Forces the next frame of `pane` (or of every pane when `None`) to be full.
    pub fn mark_full(&mut self, pane: Option<PaneId>) {
        match pane {
            Some(pane) => self.panes.entry(pane).or_default().needs_full = true,
            None => {
                for track in self.panes.values_mut() {
                    track.needs_full = true;
                }
            }
        }
    }

    /// A new `Layout` message when tabs, panes, focus, titles or geometry changed.
    pub fn layout_message(&mut self, session: &Session) -> Option<ServerMessage> {
        let current = session.layout_tabs();
        if self.layout.as_ref() == Some(&current) {
            return None;
        }
        self.revision += 1;
        let (active_tab, tabs) = current.clone();
        self.layout = Some(current);
        Some(ServerMessage::Layout(LayoutFrame {
            revision: self.revision,
            active_tab,
            tabs,
        }))
    }

    /// One frame per visible pane of the active tab whose view changed.
    pub fn frames(&mut self, session: &Session) -> Vec<PaneFrame> {
        self.panes.retain(|pane, _| session.has_pane(*pane));
        let visible = session.visible_panes();
        let mut frames = Vec::new();
        for pane in &visible {
            let Some(snapshot) = session.pane_snapshot(*pane) else {
                continue;
            };
            let newly_visible = !self.visible.contains(pane);
            let track = self.panes.entry(*pane).or_default();
            let full = track.needs_full
                || newly_visible
                || track.cols != snapshot.cols
                || track.rows.len() != snapshot.rows.len();
            let lines: Vec<attach::FrameLine> = snapshot
                .rows
                .iter()
                .enumerate()
                .filter(|(row, runs)| full || track.rows.get(*row) != Some(*runs))
                .map(|(row, runs)| attach::FrameLine {
                    row,
                    runs: runs.clone(),
                })
                .collect();
            if !full && lines.is_empty() && track.meta.as_ref() == Some(&snapshot.meta) {
                continue;
            }
            track.seq += 1;
            track.needs_full = false;
            track.cols = snapshot.cols;
            let rows = snapshot.rows.len();
            track.rows = snapshot.rows;
            track.meta = Some(snapshot.meta.clone());
            let meta = snapshot.meta;
            frames.push(PaneFrame {
                pane: pane.0,
                seq: track.seq,
                full,
                cols: snapshot.cols,
                rows,
                lines,
                cursor: meta.cursor,
                selection: meta.selection,
                modes: meta.modes,
                decorations: meta.decorations,
                title: meta.title,
                cwd: meta.cwd,
                placements: meta.placements,
            });
        }
        self.visible = visible;
        frames
    }
}

/// Most frames an [`Outbox`] holds before it discards queued frames.
pub const OUTBOX_MAX_FRAMES: usize = 64;
/// Most encoded frame bytes an [`Outbox`] holds before it discards queued frames.
pub const OUTBOX_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Most queued non-frame messages; beyond this the client is considered stuck.
pub const OUTBOX_MAX_MESSAGES: usize = 1024;
/// Image backlog limit for one client. A new image may start only while less
/// than this many bytes of earlier image lines are queued; once started, every
/// line of that image is queued, so one image larger than the budget still
/// reaches the client. Queued image bytes therefore stay below this budget plus
/// one image, which the process graphics limit already bounds. A client whose
/// backlog is still over the budget when another image must start gets an
/// error and is detached.
pub const OUTBOX_MAX_IMAGE_BYTES: usize = 256 * 1024 * 1024;

enum Outgoing {
    Frame(u64),
    Message,
    Image,
}

/// Bounded queue of encoded attach lines from the server loop to one client.
///
/// Frames are disposable: when adding a frame would exceed
/// [`OUTBOX_MAX_FRAMES`] or [`OUTBOX_MAX_FRAME_BYTES`], every queued frame is
/// dropped together with the new one, and their panes are returned so their
/// next frame is full. Layout, copy, layout-result, error, welcome and exit
/// messages are never dropped; more than [`OUTBOX_MAX_MESSAGES`] of them
/// queued means the client stopped reading and must be detached. Image lines
/// are never dropped either; they have their own byte budget,
/// [`OUTBOX_MAX_IMAGE_BYTES`], so frames referencing them stay behind them.
#[derive(Default)]
pub struct Outbox {
    queue: VecDeque<(Outgoing, Vec<u8>)>,
    frames: usize,
    frame_bytes: usize,
    messages: usize,
    image_bytes: usize,
    /// Set when frames were dropped; the writer wakes the loop once the queue drains.
    pub wake_wanted: bool,
    /// Set when the client is gone or the session ends; the writer stops once empty.
    pub closed: bool,
}

impl Outbox {
    /// Queues a frame line; returns the panes whose queued frames were dropped
    /// (including `pane` itself) when the frame did not fit.
    pub fn push_frame(&mut self, pane: u64, line: Vec<u8>) -> Vec<u64> {
        let over = self.frames >= OUTBOX_MAX_FRAMES
            || (self.frames > 0 && self.frame_bytes + line.len() > OUTBOX_MAX_FRAME_BYTES);
        if !over {
            self.frames += 1;
            self.frame_bytes += line.len();
            self.queue.push_back((Outgoing::Frame(pane), line));
            return Vec::new();
        }
        let mut dropped = BTreeSet::from([pane]);
        self.queue.retain(|(kind, _)| match kind {
            Outgoing::Frame(pane) => {
                dropped.insert(*pane);
                false
            }
            Outgoing::Message | Outgoing::Image => true,
        });
        self.frames = 0;
        self.frame_bytes = 0;
        self.wake_wanted = true;
        dropped.into_iter().collect()
    }

    /// Queues a message that is never dropped; `false` means the client is stuck.
    pub fn push_message(&mut self, line: Vec<u8>) -> bool {
        if self.messages >= OUTBOX_MAX_MESSAGES {
            return false;
        }
        self.messages += 1;
        self.queue.push_back((Outgoing::Message, line));
        true
    }

    /// Whether another image may start: earlier image lines still queued are
    /// below [`OUTBOX_MAX_IMAGE_BYTES`]. `false` means the client must be detached.
    pub fn can_start_image(&self) -> bool {
        self.image_bytes < OUTBOX_MAX_IMAGE_BYTES
    }

    /// Queues an image announcement or chunk that is never dropped. Call it for
    /// every line of an image after [`Outbox::can_start_image`] allowed the image.
    pub fn push_image(&mut self, line: Vec<u8>) {
        self.image_bytes += line.len();
        self.queue.push_back((Outgoing::Image, line));
    }

    /// Drops everything queued, e.g. before a final error to a detached client.
    pub fn discard_all(&mut self) {
        self.queue.clear();
        self.frames = 0;
        self.frame_bytes = 0;
        self.messages = 0;
        self.image_bytes = 0;
    }

    /// Next line for the writer, in queue order.
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let (kind, line) = self.queue.pop_front()?;
        match kind {
            Outgoing::Frame(_) => {
                self.frames -= 1;
                self.frame_bytes -= line.len();
            }
            Outgoing::Message => self.messages -= 1,
            Outgoing::Image => self.image_bytes -= line.len(),
        }
        Some(line)
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

fn visible_layout(tab: &Tab, pane: PaneId) -> Option<PaneLayout> {
    tab.layout()
        .into_iter()
        .find(|layout| layout.pane == pane && !layout.hidden)
}

fn spawn_pane(
    pane_id: PaneId,
    layout: &PaneLayout,
    cwd: PathBuf,
    notify: &OutputNotifier,
) -> Result<SessionPane, String> {
    let wakeup = Arc::new(WakeupGate::default());
    let notify_wakeup = Arc::clone(&wakeup);
    let notify = Arc::clone(notify);
    let pty = Pty::spawn_shell_notified(
        cwd.clone(),
        layout.cols.min(u16::MAX as u32) as u16,
        layout.rows.min(u16::MAX as u32) as u16,
        move || {
            if notify_wakeup.request() {
                notify(pane_id);
            }
        },
    )
    .map_err(|error| format!("could not start shell PTY for pane {}: {error}", pane_id.0))?;
    Ok(SessionPane {
        pty,
        model: Model::with_scrollback_limit(
            layout.cols as usize,
            layout.rows as usize,
            crate::config::current().scrollback,
        ),
        title: cwd_basename_title(&cwd),
        explicit_title: None,
        cwd,
        cwd_reported: false,
        wakeup,
        graphics: PaneGraphics::default(),
    })
}

/// Closes `pane` in `tabs`; true when it was the last pane of the last tab.
pub fn close_pane_lifecycle(tabs: &mut TabSet, pane: PaneId) -> bool {
    matches!(tabs.close_pane(pane), PaneCloseResult::LastTab { .. })
}

/// Undoes a tab added by [`create_tab_with`] whose pane could not start.
pub fn rollback_failed_new_tab(tabs: &mut TabSet) {
    if tabs.tabs().len() > 1 {
        let _ = tabs.close_active();
    }
}

/// Adds a tab and creates its first pane, rolling the tab back on failure.
pub fn create_tab_with<T>(
    tabs: &mut TabSet,
    create_pane: impl FnOnce(PaneId, PaneLayout) -> Result<T, String>,
) -> Result<(PaneId, T), String> {
    tabs.add_tab();
    let result = (|| {
        let tab = tabs
            .active()
            .ok_or_else(|| "new tab was not created".to_owned())?;
        let pane_id = tab.active_pane();
        let layout =
            visible_layout(tab, pane_id).ok_or_else(|| "new tab has no visible pane".to_owned())?;
        create_pane(pane_id, layout).map(|pane| (pane_id, pane))
    })();
    if result.is_err() {
        rollback_failed_new_tab(tabs);
    }
    result
}

/// Pane title derived from its working directory: the basename, or `so1sh`.
pub fn cwd_basename_title(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("so1sh")
        .to_owned()
}

/// An explicit OSC title wins over the directory basename.
pub fn pane_tab_title(cwd: &Path, explicit_title: Option<&str>) -> String {
    explicit_title
        .map(str::to_owned)
        .unwrap_or_else(|| cwd_basename_title(cwd))
}

/// Applies an OSC title change; returns the new pane title for title events only.
pub fn apply_title_event(
    cwd: &Path,
    explicit_title: &mut Option<String>,
    event: &ModelEvent,
) -> Option<String> {
    match event {
        ModelEvent::Title(title) => *explicit_title = Some(title.clone()),
        ModelEvent::ResetTitle => *explicit_title = None,
        _ => return None,
    }
    Some(pane_tab_title(cwd, explicit_title.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::{
        OUTBOX_MAX_FRAME_BYTES, OUTBOX_MAX_FRAMES, OUTBOX_MAX_IMAGE_BYTES, OUTBOX_MAX_MESSAGES,
        Outbox, tab_content_rect,
    };
    use crate::layout::Rect;

    #[test]
    fn tab_content_rect_reserves_one_cell_row_only_with_several_tabs() {
        let window = Rect::new(0, 0, 800, 480);
        assert_eq!(tab_content_rect(window, 1, 16), window);
        assert_eq!(tab_content_rect(window, 2, 16), Rect::new(0, 16, 800, 464));
        assert_eq!(
            tab_content_rect(Rect::new(0, 0, 800, 10), 3, 16),
            Rect::new(0, 10, 800, 0)
        );
    }

    #[test]
    fn outbox_keeps_messages_in_order_and_drops_frames_when_full() {
        let mut outbox = Outbox::default();
        assert!(outbox.push_message(b"welcome".to_vec()));
        for index in 0..OUTBOX_MAX_FRAMES {
            let pane = (index % 2) as u64;
            assert!(outbox.push_frame(pane, vec![b'f'; 8]).is_empty());
        }
        assert!(outbox.push_message(b"layout".to_vec()));
        assert!(!outbox.wake_wanted);
        // The next frame overflows: every queued frame is dropped with it.
        assert_eq!(outbox.push_frame(7, b"late".to_vec()), vec![0, 1, 7]);
        assert!(outbox.wake_wanted);
        assert_eq!(outbox.pop(), Some(b"welcome".to_vec()));
        assert_eq!(outbox.pop(), Some(b"layout".to_vec()));
        assert_eq!(outbox.pop(), None);
        assert!(outbox.is_empty());
        // Accounting was reset, so frames fit again.
        assert!(outbox.push_frame(1, b"again".to_vec()).is_empty());
        assert_eq!(outbox.pop(), Some(b"again".to_vec()));
    }

    #[test]
    fn outbox_accepts_one_oversized_frame_but_bounds_frame_bytes() {
        let mut outbox = Outbox::default();
        assert!(
            outbox
                .push_frame(1, vec![0; OUTBOX_MAX_FRAME_BYTES + 1])
                .is_empty()
        );
        assert_eq!(outbox.push_frame(2, vec![0; 1]), vec![1, 2]);
        assert!(outbox.is_empty());
    }

    #[test]
    fn outbox_never_drops_images_and_bounds_their_queued_bytes() {
        let mut outbox = Outbox::default();
        outbox.push_image(b"image".to_vec());
        for _ in 0..OUTBOX_MAX_FRAMES {
            assert!(outbox.push_frame(1, b"f".to_vec()).is_empty());
        }
        outbox.push_image(b"chunk".to_vec());
        // A frame overflow drops frames only; image lines keep their order.
        assert_eq!(outbox.push_frame(2, b"late".to_vec()), vec![1, 2]);
        assert_eq!(outbox.pop(), Some(b"image".to_vec()));
        assert_eq!(outbox.pop(), Some(b"chunk".to_vec()));
        assert!(outbox.is_empty());
        // Image lines do not count against the message limit.
        for _ in 0..OUTBOX_MAX_MESSAGES {
            assert!(outbox.push_message(b"m".to_vec()));
        }
        assert!(outbox.can_start_image());
        outbox.push_image(vec![0; OUTBOX_MAX_IMAGE_BYTES - 1]);
        assert!(outbox.can_start_image(), "just below the budget");
        outbox.push_image(vec![0; 1]);
        assert!(!outbox.can_start_image(), "the 256 MiB budget is full");
        while outbox.pop().is_some_and(|line| line.len() < 2) {}
        assert!(outbox.can_start_image(), "popping frees the budget");
        outbox.discard_all();
        assert!(outbox.is_empty());
        assert!(outbox.can_start_image());
    }

    #[test]
    fn outbox_queues_a_whole_image_larger_than_the_budget_when_no_image_is_pending() {
        let mut outbox = Outbox::default();
        // One image whose chunk lines exceed the budget: starting it is allowed
        // because nothing is queued yet, and every chunk is then accepted.
        assert!(outbox.can_start_image());
        let chunk = OUTBOX_MAX_IMAGE_BYTES / 2 + 1;
        outbox.push_image(vec![0; chunk]);
        outbox.push_image(vec![0; chunk]);
        outbox.push_image(vec![0; chunk]);
        // A further image waits until the backlog drains below the budget.
        assert!(!outbox.can_start_image(), "backlog above the budget");
        outbox.pop();
        outbox.pop();
        assert!(outbox.can_start_image(), "draining frees the budget");
    }

    #[test]
    fn outbox_reports_a_stuck_client_after_too_many_messages() {
        let mut outbox = Outbox::default();
        for _ in 0..OUTBOX_MAX_MESSAGES {
            assert!(outbox.push_message(b"m".to_vec()));
        }
        assert!(!outbox.push_message(b"m".to_vec()));
        outbox.pop();
        assert!(outbox.push_message(b"m".to_vec()));
    }

    #[test]
    fn session_socket_names_accept_only_safe_session_names() {
        use crate::control::{session_socket_name, validate_session_name};
        for valid in ["default", "a", "work_2", "agent-7", &"x".repeat(64)] {
            assert!(validate_session_name(valid).is_ok(), "{valid:?}");
        }
        for invalid in ["", "../x", "a/b", "a b", "ñ", ".", &"x".repeat(65)] {
            assert!(validate_session_name(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(session_socket_name("work"), "so1term-server-work.sock");
    }
}

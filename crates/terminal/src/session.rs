//! Headless terminal session owned by the persistent `so1term-server` (Phase 5 T10).
//!
//! A [`Session`] owns the tab/pane layout and, for every pane, the shell PTY and
//! its VT [`Model`]. It has no window, GPU, or event-loop dependency: PTY output
//! is announced through a caller-supplied [`OutputNotifier`], and control
//! requests are answered by [`Session::handle_control`]. Panes keep running while
//! no client is connected, which is what makes detach possible.
//!
//! Panes use a virtual grid of [`VIRTUAL_CELL_WIDTH`] x [`VIRTUAL_CELL_HEIGHT`]
//! pixel cells until an attached client provides real geometry (Phase 5 T11).
//! Kitty graphics ingestion is not performed by the server yet; APC sequences are
//! passed to the VT model, which ignores them.
//!
//! The title and tab helpers here are also used by the `so1term` window so both
//! frontends name panes and roll back failed tabs the same way.

use crate::control::{
    ControlCommand, ControlDirection, ControlError, ControlErrorCode, ControlOutcome,
    ControlRequest, ControlResponse, TextScope,
};
use crate::layout::{CellSize, Direction, PaneCloseResult, PaneId, PaneLayout, Rect, Tab, TabSet};
use crate::model::{Model, ModelEvent};
use crate::pty::Pty;
use crate::renderer::WakeupGate;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Width in pixels of one virtual server cell.
pub const VIRTUAL_CELL_WIDTH: u32 = 8;
/// Height in pixels of one virtual server cell.
pub const VIRTUAL_CELL_HEIGHT: u32 = 16;
/// Initial server grid columns.
pub const DEFAULT_COLS: u32 = 80;
/// Initial server grid rows.
pub const DEFAULT_ROWS: u32 = 24;

// Same layout limits as the so1term window.
const DIVIDER_WIDTH: u32 = 4;
const MIN_PANE_COLS: u32 = 20;
const MIN_PANE_ROWS: u32 = 5;

/// Called from a PTY reader thread when a pane has new output to receive.
pub type OutputNotifier = Arc<dyn Fn(PaneId) + Send + Sync>;

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
    wakeup: Arc<WakeupGate>,
}

pub struct Session {
    tabs: TabSet,
    window: Rect,
    cell: CellSize,
    panes: BTreeMap<PaneId, SessionPane>,
    start_cwd: PathBuf,
    notify: OutputNotifier,
}

impl Session {
    /// Creates the session with one tab running one shell in `cwd`.
    pub fn start(
        cols: u32,
        rows: u32,
        cwd: PathBuf,
        notify: OutputNotifier,
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
            panes: BTreeMap::from([(pane_id, pane)]),
            start_cwd: cwd,
            notify,
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
            for observed in pane.model.feed(&bytes) {
                if let crate::osc::Event::CurrentDirectory { path, .. } = observed.event {
                    pane.cwd = PathBuf::from(String::from_utf8_lossy(&path).into_owned());
                    pane.title = pane_tab_title(&pane.cwd, pane.explicit_title.as_deref());
                }
            }
        }
        for event in pane.model.take_events() {
            if let Some(title) = apply_title_event(&pane.cwd, &mut pane.explicit_title, &event) {
                pane.title = title;
                continue;
            }
            if let ModelEvent::PtyWrite(bytes) = event {
                let _ = pane.pty.write(&bytes);
            }
        }
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
                        let pane = self.panes.get(pane_id);
                        serde_json::json!({
                            "id": pane_id.0,
                            "cwd": pane.map(|pane| pane.cwd.to_string_lossy().into_owned()).unwrap_or_default(),
                            "title": pane.map(|pane| pane.title.clone()).unwrap_or_default(),
                            "focused": index == active && tab.active_pane() == *pane_id,
                        })
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
        Ok(pane_id)
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
        if last {
            self.panes.clear();
        } else {
            self.apply_layout();
        }
    }

    /// Resizes visible pane models and PTYs to the current layout.
    fn apply_layout(&mut self) {
        self.tabs.resize_all(self.window, self.cell);
        let layouts: Vec<_> = self.tabs.tabs().iter().flat_map(Tab::layout).collect();
        for layout in layouts.into_iter().filter(|layout| !layout.hidden) {
            if let Some(pane) = self.panes.get_mut(&layout.pane) {
                pane.model
                    .resize(layout.cols as usize, layout.rows as usize);
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
        model: Model::new(layout.cols as usize, layout.rows as usize),
        title: cwd_basename_title(&cwd),
        explicit_title: None,
        cwd,
        wakeup,
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

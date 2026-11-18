//! Shared kitty graphics pipeline for one pane's PTY output (Phase 5 T11d).
//!
//! The standalone so1term window and the headless `so1term-server` session run
//! the same code here, so kitty graphics behave identically in both:
//!
//! 1. [`PaneGraphics::receive`] splits PTY bytes into ordered text and APC
//!    segments ([`ApcCursorTracker`]), keeping only APC payloads the bounded
//!    [`crate::apc::Observer`] accepted.
//! 2. [`PaneGraphics::apply`] feeds text to the VT [`Model`] (with placement
//!    screen/eviction upkeep) and runs APC payloads through the
//!    [`GraphicsProtocol`]: replies go to the PTY, deletes free data, `a=p`
//!    places stored images, and transmissions start an off-thread decode.
//!    While a decode is in flight later segments wait ([`OrderedSegments`]), so
//!    the placement and its cursor movement apply before the following text.
//! 3. [`PaneGraphics::finish_decode`] stores the decoded image in the
//!    process-wide [`GraphicsStore`] (quota eviction across panes is reported
//!    by [`SharedGraphics::take_evicted`]), commits an `a=T` placement, and
//!    writes the reply.
//!
//! What differs between the frontends is behind [`GraphicsHost`]: the window
//! uploads GPU textures and decodes on a thread that wakes its event loop; the
//! server only records which images changed for attached clients and wakes
//! its loop through a channel. [`frame_placements`] projects a pane's
//! placements onto its viewport; the window renders that projection and the
//! server sends it in `PaneFrame::placements`.

use crate::apc;
use crate::attach::FramePlacement;
use crate::blocks::{GridAnchor, GridScreen};
use crate::graphics::{
    self, DecodeJob, DecodedImage, GraphicsCompletion, GraphicsError, GraphicsProtocol,
    GraphicsStore, Ingest, PlacementOptions,
};
use crate::layout::PaneId;
use crate::model::Model;
use crate::osc;
use crate::placements::{
    CellSize as PlacementCellSize, DeleteSelector, Placement, PlacementStore, SourceRect, Viewport,
};
use std::collections::VecDeque;

/// Largest APC payload the cursor tracker keeps; longer ones are discarded.
const MAX_APC_PAYLOAD: usize = 4 * 1024 * 1024;

/// Frontend-specific side of the pipeline.
pub trait GraphicsHost {
    /// Pixel size of one cell, used to size placements.
    fn cell_size(&self) -> (f32, f32);
    /// New or replaced image data was stored; returns whether it can be displayed.
    fn image_stored(&mut self, pane: PaneId, id: u32, image: &DecodedImage) -> bool;
    /// An `a=p` placement wants to show a stored image; returns whether it can be displayed.
    fn image_displayable(&mut self, pane: PaneId, id: u32, image: &DecodedImage) -> bool;
    /// The image data was freed (delete with data, or quota eviction).
    fn image_freed(&mut self, pane: PaneId, id: u32);
    /// Decodes `job` without blocking the caller, then hands the result to
    /// [`PaneGraphics::finish_decode`] for `pane` with this `anchor`.
    fn start_decode(&mut self, pane: PaneId, anchor: GridAnchor, job: DecodeJob);
}

/// Process-wide image storage shared by every pane of one frontend.
pub struct SharedGraphics {
    pub store: GraphicsStore,
    next_image_id: u32,
}

impl Default for SharedGraphics {
    fn default() -> Self {
        Self {
            store: GraphicsStore::new(graphics::PANE_LIMIT, graphics::PROCESS_LIMIT),
            next_image_id: 1,
        }
    }
}

impl SharedGraphics {
    /// Reports images evicted by the process quota to `host` and returns them;
    /// the caller then calls [`PaneGraphics::forget_evicted`] on their panes.
    pub fn take_evicted(&mut self, host: &mut dyn GraphicsHost) -> Vec<(PaneId, u32)> {
        let evicted = self.store.take_evicted();
        for (pane, id) in &evicted {
            host.image_freed(*pane, *id);
        }
        evicted
    }

    /// An unused image id for `pane`, for transmissions without `i=`.
    fn allocate_id(&mut self, pane: PaneId) -> Option<u32> {
        let start = self.next_image_id;
        let mut id = start;
        while self.store.get(pane, id).is_some() {
            id = id.wrapping_add(1);
            if id == start {
                return None;
            }
        }
        self.next_image_id = id.wrapping_add(1);
        Some(id)
    }
}

/// Per-pane graphics state: APC splitting, protocol chunks, and placements.
#[derive(Default)]
pub struct PaneGraphics {
    apc_observer: apc::Observer,
    tracker: ApcCursorTracker,
    segments: OrderedSegments,
    protocol: GraphicsProtocol,
    pub placements: PlacementStore,
}

impl PaneGraphics {
    /// Queues PTY output as ordered text and accepted APC segments.
    pub fn receive(&mut self, bytes: &[u8]) {
        let mut accepted = self.apc_observer.feed(bytes).into_iter();
        let segments = self
            .tracker
            .feed(bytes)
            .into_iter()
            .filter(|segment| match segment {
                TerminalSegment::Data(_) => true,
                TerminalSegment::Apc(payload) => accepted.next().as_ref() == Some(payload),
            });
        self.segments.extend(segments);
    }

    /// Resizes `model` and keeps placements consistent (width reflow drops them).
    pub fn resize_model(&mut self, model: &mut Model, cols: usize, rows: usize) {
        let (old_cols, old_rows) = model.grid_dimensions();
        model.resize(cols, rows);
        self.placements.resize(old_cols, old_rows, cols, rows);
    }

    /// Applies queued segments until none is left or a decode is in flight.
    ///
    /// `write` receives PTY replies; `observe` receives the OSC events the
    /// model reports while text is fed.
    #[allow(clippy::too_many_arguments)]
    pub fn apply(
        &mut self,
        pane: PaneId,
        model: &mut Model,
        shared: &mut SharedGraphics,
        host: &mut dyn GraphicsHost,
        write: &mut dyn FnMut(&[u8]),
        observe: &mut dyn FnMut(osc::Event),
    ) {
        while let Some(segment) = self.segments.next() {
            match segment {
                TerminalSegment::Data(data) => {
                    let old_screen = screen_of(model);
                    for observed in model.feed(&data) {
                        observe(observed.event);
                    }
                    self.placements.screen_changed(old_screen, screen_of(model));
                    self.placements
                        .prune_evicted_primary(model.block_row_base());
                }
                TerminalSegment::Apc(payload) => {
                    self.apply_apc(pane, &payload, model, shared, host, write)
                }
            }
        }
    }

    fn apply_apc(
        &mut self,
        pane: PaneId,
        payload: &[u8],
        model: &mut Model,
        shared: &mut SharedGraphics,
        host: &mut dyn GraphicsHost,
        write: &mut dyn FnMut(&[u8]),
    ) {
        match self.protocol.ingest(payload) {
            Ingest::Ignored | Ingest::Pending | Ingest::Reply(None) => {}
            Ingest::Reply(Some(reply)) => write(&reply),
            Ingest::Delete(command) => {
                let anchor = model.cursor_anchor();
                let mut selector = command.selector;
                let stable_top = model.visible_stable_top_row();
                let (columns, rows) = model.grid_dimensions();
                if let DeleteSelector::Cell { row, .. } | DeleteSelector::Row { row } =
                    &mut selector
                {
                    *row = row.saturating_add(stable_top);
                }
                let freed = self.placements.delete(
                    selector,
                    command.frees_data,
                    anchor,
                    anchor.screen,
                    Viewport {
                        screen: anchor.screen,
                        top_row: stable_top,
                        rows,
                        columns,
                    },
                );
                for id in freed {
                    shared.store.remove_image(pane, id);
                    host.image_freed(pane, id);
                }
                if let Some(reply) = graphics::reply(&command.response, Ok(())) {
                    write(&reply);
                }
            }
            Ingest::Place(command) => {
                let image_id = command.image_id.or_else(|| {
                    command
                        .image_number
                        .and_then(|number| self.placements.resolve_image_number(number))
                });
                let result = image_id
                    .and_then(|id| shared.store.get(pane, id).map(|image| (id, image)))
                    .ok_or_else(GraphicsError::not_found)
                    .and_then(|(id, image)| {
                        if !host.image_displayable(pane, id, image) {
                            return Err(GraphicsError::invalid_placement());
                        }
                        let anchor = model.cursor_anchor();
                        commit_placement(
                            &mut self.placements,
                            model,
                            id,
                            command.options,
                            anchor,
                            (image.width, image.height),
                            host.cell_size(),
                        )
                    });
                if let Some(reply) = graphics::reply(&command.response, result) {
                    write(&reply);
                }
            }
            Ingest::Decode(job) => {
                let anchor = model.cursor_anchor();
                self.segments.begin_decode();
                host.start_decode(pane, anchor, job);
            }
        }
    }

    /// Completes a decode started through [`GraphicsHost::start_decode`].
    ///
    /// Stores the image, commits an `a=T` placement at `anchor`, and writes the
    /// reply. Afterwards the caller handles [`SharedGraphics::take_evicted`]
    /// and calls [`Self::apply`] to resume the segments that waited.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_decode(
        &mut self,
        pane: PaneId,
        anchor: GridAnchor,
        mut completion: GraphicsCompletion,
        model: &mut Model,
        shared: &mut SharedGraphics,
        host: &mut dyn GraphicsHost,
        write: &mut dyn FnMut(&[u8]),
    ) {
        self.protocol.finish_decode();
        self.segments.finish_decode();
        let result = match completion.image {
            Ok(image) if completion.action != "q" => {
                let dimensions = (image.width, image.height);
                let id = match completion.response.image_id {
                    Some(id) => id,
                    None => match shared.allocate_id(pane) {
                        Some(id) => id,
                        // Every id is in use: drop the image without a reply.
                        None => return,
                    },
                };
                if completion.response.image_number.is_some() {
                    completion.response.image_id = Some(id);
                }
                match shared.store.insert(pane, id, image) {
                    Err(error) => Err(error),
                    Ok(()) => {
                        let displayable = shared
                            .store
                            .get(pane, id)
                            .is_some_and(|stored| host.image_stored(pane, id, stored));
                        self.placements
                            .register_image(id, completion.response.image_number);
                        self.placements
                            .retain_images(|image_id| shared.store.get(pane, image_id).is_some());
                        if completion.action != "T" {
                            Ok(())
                        } else if !displayable {
                            Err(GraphicsError::invalid_placement())
                        } else if let Some(options) = completion.placement {
                            commit_placement(
                                &mut self.placements,
                                model,
                                id,
                                options,
                                anchor,
                                dimensions,
                                host.cell_size(),
                            )
                        } else {
                            Err(GraphicsError::invalid_placement())
                        }
                    }
                }
            }
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        };
        if let Some(bytes) = graphics::reply(&completion.response, result) {
            write(&bytes);
        }
    }

    /// Drops placements of `image_id` after the process quota evicted its data.
    pub fn forget_evicted(&mut self, pane: PaneId, image_id: u32, store: &GraphicsStore) {
        self.placements
            .retain_images(|id| id != image_id || store.get(pane, id).is_some());
    }
}

fn screen_of(model: &Model) -> GridScreen {
    if model.alternate_screen_active() {
        GridScreen::Alternate
    } else {
        GridScreen::Primary
    }
}

/// Projects `placements` onto `model`'s viewport in drawing order (z, then
/// placement order). Rows are relative to the top visible row.
pub fn frame_placements(placements: &PlacementStore, model: &Model) -> Vec<FramePlacement> {
    let top_row = model.visible_stable_top_row();
    let (columns, rows) = model.grid_dimensions();
    placements
        .visible(Viewport {
            screen: screen_of(model),
            top_row,
            rows,
            columns,
        })
        .into_iter()
        .map(|entry| {
            let placement = entry.placement;
            FramePlacement {
                image: placement.image_id,
                column: placement.anchor.column,
                row: placement
                    .anchor
                    .line
                    .saturating_sub(top_row)
                    .clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                columns: placement.cells.columns,
                rows: placement.cells.rows,
                x_offset: placement.pixel_offset.0,
                y_offset: placement.pixel_offset.1,
                source_x: placement.source.x,
                source_y: placement.source.y,
                source_width: placement.source.width,
                source_height: placement.source.height,
                z: placement.z,
            }
        })
        .collect()
}

/// PTY output split at APC boundaries, in stream order.
pub(crate) enum TerminalSegment {
    Data(Vec<u8>),
    Apc(Vec<u8>),
}

/// PTY segments in stream order. While an image decode is in flight, later
/// segments wait so placement and its cursor movement apply before the text
/// that follows them, as if the decode had been synchronous.
#[derive(Default)]
pub(crate) struct OrderedSegments {
    pending: VecDeque<TerminalSegment>,
    decode_in_flight: bool,
}

impl OrderedSegments {
    pub fn extend(&mut self, segments: impl IntoIterator<Item = TerminalSegment>) {
        self.pending.extend(segments);
    }

    pub fn next(&mut self) -> Option<TerminalSegment> {
        if self.decode_in_flight {
            return None;
        }
        self.pending.pop_front()
    }

    pub fn begin_decode(&mut self) {
        self.decode_in_flight = true;
    }

    pub fn finish_decode(&mut self) {
        self.decode_in_flight = false;
    }
}

/// Splits PTY output at `ESC _ ... ESC \` boundaries so APC payloads apply at
/// their position in the text stream (and therefore at the right cursor).
#[derive(Default)]
pub(crate) struct ApcCursorTracker {
    state: ApcCursorState,
    payload: Vec<u8>,
    discarding: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ApcCursorState {
    #[default]
    Ground,
    Escape,
    Apc,
    ApcEscape,
    Discard,
    DiscardEscape,
}

impl ApcCursorTracker {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<TerminalSegment> {
        let mut segments = Vec::new();
        let mut data = Vec::new();
        for &byte in bytes {
            match self.state {
                ApcCursorState::Ground => {
                    if byte == 0x1b {
                        self.state = ApcCursorState::Escape;
                    } else {
                        data.push(byte);
                    }
                }
                ApcCursorState::Escape => {
                    if byte == b'_' {
                        self.payload.clear();
                        self.discarding = false;
                        self.state = ApcCursorState::Apc;
                    } else {
                        data.extend_from_slice(&[0x1b, byte]);
                        self.state = if byte == 0x1b {
                            ApcCursorState::Escape
                        } else {
                            ApcCursorState::Ground
                        };
                    }
                }
                ApcCursorState::Apc => match byte {
                    0x18 | 0x1a => {
                        self.payload.clear();
                        self.discarding = false;
                        self.state = ApcCursorState::Ground;
                    }
                    0x1b => self.state = ApcCursorState::ApcEscape,
                    byte => {
                        if !self.discarding {
                            self.payload.push(byte);
                            if self.payload.len() > MAX_APC_PAYLOAD {
                                self.payload.clear();
                                self.discarding = true;
                                self.state = ApcCursorState::Discard;
                            }
                        }
                    }
                },
                ApcCursorState::ApcEscape => {
                    if byte == b'\\' {
                        if !self.discarding {
                            if !data.is_empty() {
                                segments.push(TerminalSegment::Data(std::mem::take(&mut data)));
                            }
                            segments.push(TerminalSegment::Apc(std::mem::take(&mut self.payload)));
                        }
                        self.payload.clear();
                        self.discarding = false;
                        self.state = ApcCursorState::Ground;
                    } else if byte == b'_' {
                        self.payload.clear();
                        self.discarding = false;
                        self.state = ApcCursorState::Apc;
                    } else if byte != 0x1b {
                        self.payload.clear();
                        self.discarding = false;
                        self.state = ApcCursorState::Ground;
                    }
                }
                ApcCursorState::Discard => {
                    if byte == 0x18 || byte == 0x1a {
                        self.discarding = false;
                        self.state = ApcCursorState::Ground;
                    } else if byte == 0x1b {
                        self.state = ApcCursorState::DiscardEscape;
                    }
                }
                ApcCursorState::DiscardEscape => {
                    if byte == b'\\' {
                        self.discarding = false;
                        self.state = ApcCursorState::Ground;
                    } else if byte == b'_' {
                        self.payload.clear();
                        self.discarding = false;
                        self.state = ApcCursorState::Apc;
                    } else if byte != 0x1b {
                        self.state = ApcCursorState::Discard;
                    }
                }
            }
        }
        if !data.is_empty() {
            segments.push(TerminalSegment::Data(data));
        }
        segments
    }
}

/// Builds a placement from kitty options: clips the source rectangle to the
/// image and derives a missing cell dimension from the aspect ratio.
pub fn placement_from_options(
    image_id: u32,
    options: PlacementOptions,
    anchor: GridAnchor,
    image: (u32, u32),
    cell: (f32, f32),
) -> Result<Placement, GraphicsError> {
    let (image_width, image_height) = image;
    let x = options.source.0.min(image_width);
    let y = options.source.1.min(image_height);
    let width = if options.source.2 == 0 {
        image_width.saturating_sub(x)
    } else {
        options.source.2.min(image_width.saturating_sub(x))
    };
    let height = if options.source.3 == 0 {
        image_height.saturating_sub(y)
    } else {
        options.source.3.min(image_height.saturating_sub(y))
    };
    if width == 0 || height == 0 || cell.0 <= 0.0 || cell.1 <= 0.0 {
        return Err(GraphicsError::invalid_placement());
    }
    let columns = options.cells.0.unwrap_or_else(|| {
        ((options
            .cells
            .1
            .unwrap_or_else(|| ((height as f32 / cell.1).ceil() as usize).max(1)) as f32
            * cell.1
            * width as f32
            / height as f32
            / cell.0)
            .ceil() as usize)
            .max(1)
    });
    let rows = options.cells.1.unwrap_or_else(|| {
        ((options
            .cells
            .0
            .unwrap_or_else(|| ((width as f32 / cell.0).ceil() as usize).max(1)) as f32
            * cell.0
            * height as f32
            / width as f32
            / cell.1)
            .ceil() as usize)
            .max(1)
    });
    if options.offset.0 as f32 >= cell.0 || options.offset.1 as f32 >= cell.1 {
        return Err(GraphicsError::invalid_placement());
    }
    Ok(Placement::new(
        image_id,
        options.placement_id,
        anchor,
        SourceRect {
            x,
            y,
            width,
            height,
        },
        PlacementCellSize { columns, rows },
        options.offset,
        options.z,
    ))
}

/// Moves the cursor past the bottom-right cell of a placement unless `C=1`.
pub fn move_cursor_after_placement(model: &mut Model, cells: (usize, usize), no_cursor_move: bool) {
    if no_cursor_move || cells.0 == 0 || cells.1 == 0 {
        return;
    }
    let mut sequence = Vec::new();
    let down = cells.1.saturating_sub(1);
    if down > 0 {
        sequence.extend_from_slice(format!("\x1b[{down}B").as_bytes());
    }
    if cells.0 > 0 {
        sequence.extend_from_slice(format!("\x1b[{}C", cells.0).as_bytes());
    }
    model.feed(&sequence);
}

fn commit_placement(
    store: &mut PlacementStore,
    model: &mut Model,
    image_id: u32,
    options: PlacementOptions,
    anchor: GridAnchor,
    image: (u32, u32),
    cell: (f32, f32),
) -> Result<(), GraphicsError> {
    let placement = placement_from_options(image_id, options, anchor, image, cell)?;
    let size = (placement.cells.columns, placement.cells.rows);
    store.place(placement);
    move_cursor_after_placement(model, size, options.no_cursor_move);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::GridScreen;

    /// Host that decodes synchronously on the test thread when asked.
    #[derive(Default)]
    struct TestHost {
        displayable: bool,
        stored: Vec<(PaneId, u32)>,
        freed: Vec<(PaneId, u32)>,
        jobs: Vec<(PaneId, GridAnchor, DecodeJob)>,
    }

    impl GraphicsHost for TestHost {
        fn cell_size(&self) -> (f32, f32) {
            (8.0, 16.0)
        }
        fn image_stored(&mut self, pane: PaneId, id: u32, _image: &DecodedImage) -> bool {
            self.stored.push((pane, id));
            self.displayable
        }
        fn image_displayable(&mut self, _pane: PaneId, _id: u32, _image: &DecodedImage) -> bool {
            self.displayable
        }
        fn image_freed(&mut self, pane: PaneId, id: u32) {
            self.freed.push((pane, id));
        }
        fn start_decode(&mut self, pane: PaneId, anchor: GridAnchor, job: DecodeJob) {
            self.jobs.push((pane, anchor, job));
        }
    }

    struct Harness {
        pane: PaneGraphics,
        model: Model,
        shared: SharedGraphics,
        host: TestHost,
        replies: Vec<u8>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                pane: PaneGraphics::default(),
                model: Model::new(20, 6),
                shared: SharedGraphics::default(),
                host: TestHost {
                    displayable: true,
                    ..TestHost::default()
                },
                replies: Vec::new(),
            }
        }

        /// Feeds output and completes every decode it starts, like the frontends' loops.
        fn output(&mut self, bytes: &[u8]) {
            let pane = PaneId(1);
            self.pane.receive(bytes);
            loop {
                let replies = &mut self.replies;
                self.pane.apply(
                    pane,
                    &mut self.model,
                    &mut self.shared,
                    &mut self.host,
                    &mut |reply| replies.extend_from_slice(reply),
                    &mut |_| {},
                );
                let Some((pane, anchor, job)) = self.host.jobs.pop() else {
                    break;
                };
                let completion = graphics::decode_transfer(job);
                let replies = &mut self.replies;
                self.pane.finish_decode(
                    pane,
                    anchor,
                    completion,
                    &mut self.model,
                    &mut self.shared,
                    &mut self.host,
                    &mut |reply| replies.extend_from_slice(reply),
                );
            }
        }

        fn placements(&self) -> Vec<FramePlacement> {
            frame_placements(&self.pane.placements, &self.model)
        }
    }

    /// One opaque red RGBA pixel, base64.
    const RED_PIXEL: &str = "/wAA/w==";

    #[test]
    fn transmit_and_place_stores_the_image_replies_ok_and_projects_the_placement() {
        let mut harness = Harness::new();
        harness.output(format!("ab\x1b_Ga=T,f=32,s=1,v=1,i=7;{RED_PIXEL}\x1b\\cd").as_bytes());
        assert_eq!(harness.replies, b"\x1b_Gi=7;OK\x1b\\");
        assert_eq!(harness.host.stored, vec![(PaneId(1), 7)]);
        let image = harness.shared.store.get(PaneId(1), 7).unwrap();
        assert_eq!((image.width, image.height), (1, 1));
        assert_eq!(image.rgba, vec![255, 0, 0, 255]);
        assert_eq!(
            harness.placements(),
            vec![FramePlacement {
                image: 7,
                column: 2,
                row: 0,
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
        // The cursor moved past the image before the following text was fed.
        assert_eq!(
            harness
                .model
                .visible_text()
                .lines()
                .next()
                .unwrap()
                .trim_end(),
            "ab  cd"
        );
    }

    #[test]
    fn uppercase_delete_frees_data_and_reports_it_to_the_host() {
        let mut harness = Harness::new();
        harness.output(format!("\x1b_Ga=T,f=32,s=1,v=1,i=3,q=1;{RED_PIXEL}\x1b\\").as_bytes());
        assert_eq!(harness.placements().len(), 1);
        harness.output(b"\x1b_Ga=d,d=a\x1b\\");
        assert!(harness.placements().is_empty());
        assert!(harness.shared.store.get(PaneId(1), 3).is_some());
        assert!(harness.host.freed.is_empty());
        harness.output(b"\x1b_Ga=p,i=3,q=1\x1b\\");
        assert_eq!(harness.placements().len(), 1);
        harness.output(b"\x1b_Ga=d,d=A\x1b\\");
        assert!(harness.placements().is_empty());
        assert!(harness.shared.store.get(PaneId(1), 3).is_none());
        assert_eq!(harness.host.freed, vec![(PaneId(1), 3)]);
        assert!(harness.replies.is_empty(), "q=1 suppresses OK replies");
    }

    #[test]
    fn undisplayable_images_refuse_placement_like_a_failed_texture_upload() {
        let mut harness = Harness::new();
        harness.host.displayable = false;
        harness.output(format!("\x1b_Ga=T,f=32,s=1,v=1,i=4;{RED_PIXEL}\x1b\\").as_bytes());
        assert!(harness.placements().is_empty());
        let reply = String::from_utf8(harness.replies.clone()).unwrap();
        assert!(reply.starts_with("\x1b_Gi=4;EINVAL"), "{reply:?}");
        assert!(harness.shared.store.get(PaneId(1), 4).is_some());
    }

    #[test]
    fn missing_image_placement_replies_not_found() {
        let mut harness = Harness::new();
        harness.output(b"\x1b_Ga=p,i=99\x1b\\");
        let reply = String::from_utf8(harness.replies.clone()).unwrap();
        assert!(reply.starts_with("\x1b_Gi=99;ENOENT"), "{reply:?}");
    }

    #[test]
    fn width_reflow_drops_placements_and_rows_follow_scrollback() {
        let mut harness = Harness::new();
        harness.output(format!("\x1b_Ga=T,f=32,s=1,v=1,i=5,q=1;{RED_PIXEL}\x1b\\").as_bytes());
        harness.output(b"\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n");
        assert_eq!(harness.placements(), Vec::new(), "scrolled out of view");
        harness.model.scroll_up(2);
        assert_eq!(harness.placements()[0].row, 0);
        harness.model.scroll_to_bottom();
        harness.pane.resize_model(&mut harness.model, 20, 8);
        harness.model.scroll_up(1);
        assert_eq!(
            harness.placements().len(),
            1,
            "height change keeps placements"
        );
        harness.pane.resize_model(&mut harness.model, 30, 8);
        assert!(harness.pane.placements.placements().is_empty());
    }

    #[test]
    fn quota_eviction_reports_freed_images_and_forgets_their_placements() {
        let mut harness = Harness::new();
        harness.shared.store = GraphicsStore::new(4, 4);
        harness.output(format!("\x1b_Ga=T,f=32,s=1,v=1,i=1,q=1;{RED_PIXEL}\x1b\\").as_bytes());
        harness.output(format!("\x1b_Ga=T,f=32,s=1,v=1,i=2,q=1;{RED_PIXEL}\x1b\\").as_bytes());
        let evicted = harness.shared.take_evicted(&mut harness.host);
        assert_eq!(evicted, vec![(PaneId(1), 1)]);
        assert_eq!(harness.host.freed, vec![(PaneId(1), 1)]);
        for (pane, id) in evicted {
            harness.pane.forget_evicted(pane, id, &harness.shared.store);
        }
        let images: Vec<u32> = harness.placements().iter().map(|p| p.image).collect();
        assert_eq!(images, vec![2]);
    }

    #[test]
    fn image_cursor_policy_moves_to_after_bottom_right_only_when_requested() {
        let mut model = Model::new(10, 5);
        move_cursor_after_placement(&mut model, (3, 2), false);
        assert_eq!((model.cursor().column, model.cursor().row), (3, 1));
        move_cursor_after_placement(&mut model, (2, 2), true);
        assert_eq!((model.cursor().column, model.cursor().row), (3, 1));
    }

    #[test]
    fn apc_boundaries_preserve_text_order_and_cursor_anchor_across_chunk_splits() {
        let mut tracker = ApcCursorTracker::default();
        let first = tracker.feed(b"before\x1b_Ga=p,i=7;");
        assert!(matches!(&first[..], [TerminalSegment::Data(bytes)] if bytes == b"before"));
        let second = tracker.feed(b"payload\x1b\\after");
        assert!(
            matches!(&second[..], [TerminalSegment::Apc(payload), TerminalSegment::Data(bytes)] if payload == b"Ga=p,i=7;payload" && bytes == b"after")
        );
    }

    #[test]
    fn segments_after_an_in_flight_decode_wait_until_it_finishes() {
        let mut queue = OrderedSegments::default();
        queue.extend([
            TerminalSegment::Data(b"before".to_vec()),
            TerminalSegment::Apc(b"Ga=T;image".to_vec()),
            TerminalSegment::Data(b"after".to_vec()),
        ]);
        assert!(matches!(queue.next(), Some(TerminalSegment::Data(bytes)) if bytes == b"before"));
        assert!(matches!(queue.next(), Some(TerminalSegment::Apc(_))));
        queue.begin_decode();
        queue.extend([TerminalSegment::Data(b"later".to_vec())]);
        assert!(queue.next().is_none());
        queue.finish_decode();
        assert!(matches!(queue.next(), Some(TerminalSegment::Data(bytes)) if bytes == b"after"));
        assert!(matches!(queue.next(), Some(TerminalSegment::Data(bytes)) if bytes == b"later"));
        assert!(queue.next().is_none());
    }

    #[test]
    fn placement_geometry_clips_source_and_derives_missing_display_dimension() {
        let placement = placement_from_options(
            9,
            PlacementOptions {
                source: (10, 0, 0, 20),
                cells: (Some(4), None),
                ..Default::default()
            },
            GridAnchor {
                line: 2,
                column: 3,
                screen: GridScreen::Primary,
            },
            (100, 20),
            (8.0, 16.0),
        )
        .unwrap();
        assert_eq!(
            placement.source,
            SourceRect {
                x: 10,
                y: 0,
                width: 90,
                height: 20
            }
        );
        assert_eq!((placement.cells.columns, placement.cells.rows), (4, 1));
        assert!(
            placement_from_options(
                9,
                PlacementOptions {
                    offset: (8, 0),
                    ..Default::default()
                },
                placement.anchor,
                (100, 20),
                (8.0, 16.0)
            )
            .is_err()
        );
    }
}

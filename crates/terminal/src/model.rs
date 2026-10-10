//! Headless VT model adapter. The same byte stream is observed for shell OSCs
//! and consumed by Alacritty's persistent ANSI parser.
use std::cell::RefCell;
use std::rc::Rc;

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionRange, SelectionType},
    term::{
        Config, LineDamageBounds, Term,
        cell::{Cell, Flags},
    },
};
use vte::ansi::{Processor, Rgb};

macro_rules! delegate_handler {
    ($($method:ident($($name:ident: $ty:ty),*);)*) => {
        $(fn $method(&mut self, $($name: $ty),*) {
            self.term.$method($($name),*);
        })*
    };
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModelEvent {
    Title(String),
    ResetTitle,
    PtyWrite(Vec<u8>),
    Bell,
    Other(String),
}

#[derive(Clone, Default)]
struct Listener(Rc<RefCell<Vec<TermEvent>>>);
impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        self.0.borrow_mut().push(event);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellColor {
    Rgb(u8, u8, u8),
    Indexed(u8),
    Named(u16),
}
#[derive(Clone, Debug, PartialEq)]
pub struct VisibleCell {
    pub character: char,
    pub foreground: CellColor,
    pub background: CellColor,
    pub bold: bool,
    pub inverse: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub column: usize,
    pub row: usize,
    pub shape: vte::ansi::CursorShape,
}

pub struct Model {
    term: Term<Listener>,
    processor: Processor,
    listener: Listener,
    osc: crate::osc::Observer,
    blocks: crate::blocks::BlockList,
    /// Total input bytes already consumed by the observer; maps absolute OSC offsets to chunks.
    stream_bytes_seen: u64,
    /// Maximum primary-grid history rows retained by alacritty_terminal.
    scrollback_limit: usize,
    /// Number of primary history rows actually evicted since the model was created.
    evicted_rows: u64,
    /// Epoch offset preserves monotonic row identities when ED 2 or RIS clears the grid.
    stable_row_base: u64,
    /// Current DECSTBM region, tracked because Term does not expose it.
    scroll_region: std::ops::Range<usize>,
    selection_anchor: Option<Point>,
    selection_anchor_is_start: bool,
    selection_kind: Option<SelectionType>,
    selection_snapshot: Option<String>,
    selected_block_id: Option<u64>,
    /// Optional absolute row used to align a selected block inside the live grid.
    viewport_top_line: Option<i64>,
}

/// VTE handler proxy that delegates every operation to Term while observing
/// only operations which can push primary rows out of its bounded history.
struct EvictionTracker<'a> {
    term: &'a mut Term<Listener>,
    blocks: &'a mut crate::blocks::BlockList,
    evicted_rows: &'a mut u64,
    stable_row_base: &'a mut u64,
    history_limit: usize,
    region: std::ops::Range<usize>,
}

impl EvictionTracker<'_> {
    fn is_full_primary_region(&self) -> bool {
        self.region.start == 0
            && self.region.end == self.term.screen_lines()
            && !self
                .term
                .mode()
                .contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
    }

    fn record_eviction(&mut self, rows: usize) {
        if rows == 0 || !self.is_full_primary_region() {
            return;
        }
        let history = self.term.grid().history_size();
        self.record_eviction_from(history, rows);
    }

    fn record_eviction_from(&mut self, history: usize, rows: usize) {
        let evicted = (history + rows)
            .saturating_sub(self.history_limit)
            .min(rows);
        *self.evicted_rows = self.evicted_rows.saturating_add(evicted as u64);
    }

    fn linefeed_scrolls_primary(&self) -> bool {
        self.is_full_primary_region()
            && self.term.grid().cursor.point.line.0 + 1 == self.region.end as i32
    }

    fn advance_row_epoch(&mut self) {
        self.advance_row_epoch_from(self.term.grid().history_size());
    }

    fn advance_row_epoch_from(&mut self, history_size: usize) {
        let span = self
            .evicted_rows
            .saturating_add(history_size as u64)
            .saturating_add(self.term.screen_lines() as u64)
            .saturating_add(1);
        *self.stable_row_base = self.stable_row_base.saturating_add(span);
    }
}

impl vte::ansi::Handler for EvictionTracker<'_> {
    fn input(&mut self, character: char) {
        let cursor = &self.term.grid().cursor;
        let pending_wrap = cursor.input_needs_wrap;
        let bottom_row = cursor.point.line.0 + 1 == self.region.end as i32;
        let character_width = crate::renderer::unicode_cell_width(character);
        let can_wrap = self.is_full_primary_region()
            && bottom_row
            && self
                .term
                .mode()
                .contains(alacritty_terminal::term::TermMode::LINE_WRAP);
        let causes_scroll = can_wrap && character_width > 0 && pending_wrap;
        let history_before = self.term.grid().history_size();
        self.term.input(character);
        if causes_scroll {
            self.record_eviction_from(history_before, 1);
        }
    }

    fn linefeed(&mut self) {
        if self.linefeed_scrolls_primary() {
            self.record_eviction(1);
        }
        self.term.linefeed();
    }

    fn newline(&mut self) {
        if self.linefeed_scrolls_primary() {
            self.record_eviction(1);
        }
        self.term.newline();
    }

    fn scroll_up(&mut self, rows: usize) {
        let bounded = rows.min(self.region.end.saturating_sub(self.region.start));
        self.record_eviction(bounded);
        self.term.scroll_up(rows);
    }

    fn delete_lines(&mut self, rows: usize) {
        if self.is_full_primary_region() && self.term.grid().cursor.point.line.0 == 0 {
            self.record_eviction(rows.min(self.term.screen_lines()));
        }
        self.term.delete_lines(rows);
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        let bottom = bottom.unwrap_or_else(|| self.term.screen_lines());
        if top < bottom {
            self.region = top.saturating_sub(1).min(self.term.screen_lines())
                ..bottom.min(self.term.screen_lines());
        }
        self.term.set_scrolling_region(top, Some(bottom));
    }

    fn reset_state(&mut self) {
        self.advance_row_epoch();
        self.term.reset_state();
        self.blocks.invalidate_primary_anchors();
        self.region = 0..self.term.screen_lines();
    }

    fn clear_screen(&mut self, mode: vte::ansi::ClearMode) {
        let primary = !self
            .term
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN);
        let saved = matches!(&mode, vte::ansi::ClearMode::Saved);
        let all = matches!(&mode, vte::ansi::ClearMode::All);
        let history_before = self.term.grid().history_size();
        self.term.clear_screen(mode);

        if !primary {
            return;
        }
        if saved {
            let removed = history_before.saturating_sub(self.term.grid().history_size());
            *self.evicted_rows = self.evicted_rows.saturating_add(removed as u64);
            if removed > 0 {
                self.blocks.expire_primary_anchors_before(
                    self.stable_row_base.saturating_add(*self.evicted_rows),
                );
            }
        } else if all {
            // ED 2 destroys the visible primary rows. Start a fresh monotonic
            // row-coordinate epoch after invalidating their anchors.
            self.blocks.invalidate_primary_anchors();
            self.advance_row_epoch_from(history_before);
        }
    }

    delegate_handler! {
        set_title(arg0: Option<String>);
        set_cursor_style(arg0: Option<vte::ansi::CursorStyle>);
        set_cursor_shape(_shape: vte::ansi::CursorShape);
        goto(_line: i32, _col: usize);
        goto_line(_line: i32);
        goto_col(_col: usize);
        insert_blank(arg0: usize);
        move_up(arg0: usize);
        move_down(arg0: usize);
        identify_terminal(_intermediate: Option<char>);
        device_status(arg0: usize);
        move_forward(_col: usize);
        move_backward(_col: usize);
        move_down_and_cr(_row: usize);
        move_up_and_cr(_row: usize);
        put_tab(_count: u16);
        backspace();
        carriage_return();
        bell();
        substitute();
        set_horizontal_tabstop();
        scroll_down(arg0: usize);
        insert_blank_lines(arg0: usize);
        erase_chars(arg0: usize);
        delete_chars(arg0: usize);
        move_backward_tabs(_count: u16);
        move_forward_tabs(_count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(_mode: vte::ansi::LineClearMode);
        clear_tabs(_mode: vte::ansi::TabulationClearMode);
        set_tabs(_interval: u16);
        reverse_index();
        terminal_attribute(_attr: vte::ansi::Attr);
        set_mode(_mode: vte::ansi::Mode);
        unset_mode(_mode: vte::ansi::Mode);
        report_mode(_mode: vte::ansi::Mode);
        set_private_mode(_mode: vte::ansi::PrivateMode);
        unset_private_mode(_mode: vte::ansi::PrivateMode);
        report_private_mode(_mode: vte::ansi::PrivateMode);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(arg0: vte::ansi::CharsetIndex);
        configure_charset(arg0: vte::ansi::CharsetIndex, arg1: vte::ansi::StandardCharset);
        set_color(arg0: usize, arg1: vte::ansi::Rgb);
        dynamic_color_sequence(arg0: String, arg1: usize, arg2: &str);
        reset_color(arg0: usize);
        clipboard_store(arg0: u8, arg1: &[u8]);
        clipboard_load(arg0: u8, arg1: &str);
        decaln();
        push_title();
        pop_title();
        text_area_size_pixels();
        text_area_size_chars();
        set_hyperlink(arg0: Option<vte::ansi::Hyperlink>);
        set_mouse_cursor_icon(arg0: winit::window::CursorIcon);
        report_keyboard_mode();
        push_keyboard_mode(_mode: vte::ansi::KeyboardModes);
        pop_keyboard_modes(_to_pop: u16);
        set_keyboard_mode(_mode: vte::ansi::KeyboardModes, _behavior: vte::ansi::KeyboardModesApplyBehavior);
        set_modify_other_keys(_mode: vte::ansi::ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(_char_path: vte::ansi::ScpCharPath, _update_mode: vte::ansi::ScpUpdateMode);
    }
}

#[derive(Clone, Copy)]
struct Size {
    cols: usize,
    rows: usize,
}
impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

impl Model {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self::with_scrollback_limit(cols, rows, 10_000)
    }

    fn with_scrollback_limit(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
        let listener = Listener::default();
        let config = Config {
            scrolling_history: scrollback_limit,
            ..Config::default()
        };
        let term = Term::new(
            config,
            &Size {
                cols: cols.max(1),
                rows: rows.max(1),
            },
            listener.clone(),
        );
        Self {
            term,
            processor: Processor::new(),
            listener,
            osc: crate::osc::Observer::new(),
            blocks: crate::blocks::BlockList::default(),
            stream_bytes_seen: 0,
            scrollback_limit,
            evicted_rows: 0,
            stable_row_base: 0,
            scroll_region: 0..rows.max(1),
            selection_anchor: None,
            selection_anchor_is_start: true,
            selection_kind: None,
            selection_snapshot: None,
            selected_block_id: None,
            viewport_top_line: None,
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<crate::osc::ObservedEvent> {
        let previous_epoch = self.stable_row_base;
        let mut selected_damage = false;
        let mut full_damage = false;
        let events = self.osc.feed(bytes);
        let stream_start = self.stream_bytes_seen;
        let mut consumed = 0usize;
        let mut offscreen_mutation = false;
        for event in &events {
            let end = event
                .end_offset
                .saturating_sub(stream_start)
                .min(bytes.len() as u64) as usize;
            let end = end.max(consumed);
            let (intersects, full, mutation) = self.advance_segment(&bytes[consumed..end]);
            selected_damage |= intersects;
            full_damage |= full;
            offscreen_mutation |= mutation;
            consumed = end;

            let anchor = matches!(&event.event, crate::osc::Event::SemanticMark(_))
                .then(|| self.grid_anchor());
            self.blocks.consume_event(event, anchor);
        }
        let (intersects, full, mutation) = self.advance_segment(&bytes[consumed..]);
        selected_damage |= intersects;
        full_damage |= full;
        offscreen_mutation |= mutation;
        self.stream_bytes_seen = self.stream_bytes_seen.saturating_add(bytes.len() as u64);
        if self.stable_row_base != previous_epoch {
            self.viewport_top_line = None;
        } else if let Some(top) = self.viewport_top_line.as_mut() {
            let earliest = self.stable_row_base.saturating_add(self.evicted_rows) as i64;
            *top = (*top).max(earliest);
        }

        if !bytes.is_empty() && self.term.selection.is_some() {
            let range = self
                .term
                .selection
                .as_ref()
                .and_then(|selection| selection.to_range(&self.term));
            let offset = self.term.grid().display_offset();
            let partially_offscreen = range.is_some_and(|range| {
                selection_outside_viewport(range, offset, self.term.grid().screen_lines())
            });
            if (selected_damage || (partially_offscreen && (full_damage || offscreen_mutation)))
                && self.term.selection_to_string() != self.selection_snapshot
            {
                self.clear_selection();
            }
        } else if !bytes.is_empty() {
            self.term.reset_damage();
        }
        events
    }

    fn advance_segment(&mut self, bytes: &[u8]) -> (bool, bool, bool) {
        if bytes.is_empty() {
            return (false, false, false);
        }
        let previous_range = self
            .term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term));
        let evicted_before = self.evicted_rows;
        let mut tracker = EvictionTracker {
            term: &mut self.term,
            blocks: &mut self.blocks,
            evicted_rows: &mut self.evicted_rows,
            stable_row_base: &mut self.stable_row_base,
            history_limit: self.scrollback_limit,
            region: self.scroll_region.clone(),
        };
        self.processor.advance(&mut tracker, bytes);
        self.scroll_region = tracker.region;
        if self.evicted_rows > evicted_before {
            self.blocks.expire_primary_anchors_before(
                self.stable_row_base.saturating_add(self.evicted_rows),
            );
        }
        let range = self
            .term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term));
        if let (Some(previous), Some(current), Some(anchor)) =
            (previous_range, range, self.selection_anchor.as_mut())
        {
            let previous_anchor_line = if self.selection_anchor_is_start {
                previous.start.line
            } else {
                previous.end.line
            };
            let current_anchor_line = if self.selection_anchor_is_start {
                current.start.line
            } else {
                current.end.line
            };
            anchor.line += current_anchor_line - previous_anchor_line;
        }
        let offset = self.term.grid().display_offset();
        // Alacritty filters damage in history while scrolled away from the
        // bottom. For that case, compare selected text only when this segment
        // contains terminal data outside OSC metadata, not for every feed.
        let mutation = contains_non_osc_terminal_data(bytes);
        let (full, intersects) = match self.term.damage() {
            alacritty_terminal::term::TermDamage::Full => (true, true),
            alacritty_terminal::term::TermDamage::Partial(damage) => (
                false,
                range.is_some_and(|range| {
                    damage
                        .into_iter()
                        .any(|line| damage_intersects_selection(line, offset, range))
                }),
            ),
        };
        self.term.reset_damage();
        (intersects, full, mutation)
    }

    fn grid_anchor(&self) -> crate::blocks::GridAnchor {
        use crate::blocks::{GridAnchor, GridScreen};
        let alternate = self
            .term
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN);
        let grid = self.term.grid();
        let point = grid.cursor.point;
        GridAnchor {
            line: if alternate {
                point.line.0.max(0) as i64
            } else {
                self.stable_row_base as i64
                    + self.evicted_rows as i64
                    + grid.history_size() as i64
                    + point.line.0 as i64
            },
            column: point.column.0,
            screen: if alternate {
                GridScreen::Alternate
            } else {
                GridScreen::Primary
            },
        }
    }

    pub fn blocks(&self) -> &crate::blocks::BlockList {
        &self.blocks
    }

    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    pub fn evicted_rows(&self) -> u64 {
        self.evicted_rows
    }

    pub fn block_row_base(&self) -> u64 {
        self.stable_row_base.saturating_add(self.evicted_rows)
    }

    pub fn block_viewport_top(&self) -> i64 {
        self.viewport_top_line.unwrap_or_else(|| {
            (self.stable_row_base + self.evicted_rows + self.term.grid().history_size() as u64)
                as i64
                - self.display_offset() as i64
        })
    }

    pub fn alternate_screen_active(&self) -> bool {
        self.term
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if self.term.columns() != cols || self.term.screen_lines() != rows {
            self.blocks.invalidate_anchors();
            self.viewport_top_line = None;
        }
        self.term.resize(Size { cols, rows });
        self.scroll_region = 0..rows;
    }
    pub fn application_cursor_keys(&self) -> bool {
        self.term
            .mode()
            .contains(alacritty_terminal::term::TermMode::APP_CURSOR)
    }
    pub fn bracketed_paste_enabled(&self) -> bool {
        self.term
            .mode()
            .contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE)
    }
    pub fn mouse_or_alternate_screen_active(&self) -> bool {
        let mode = self.term.mode();
        mode.intersects(
            alacritty_terminal::term::TermMode::MOUSE_MODE
                | alacritty_terminal::term::TermMode::ALT_SCREEN,
        )
    }
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
        self.viewport_top_line = None;
    }

    pub fn navigate_block(&mut self, direction: crate::block_navigation::NavigationDirection) {
        let Some(target) = crate::block_navigation::navigation_target(
            &self.blocks,
            self.block_viewport_top(),
            direction,
        ) else {
            return;
        };
        let min_line = self.block_row_base() as i64;
        let max_line =
            min_line + self.term.grid().history_size() as i64 + self.term.screen_lines() as i64 - 1;
        self.viewport_top_line = Some(target.clamp(min_line, max_line));
    }

    pub fn select_block_at_viewport(&mut self) -> bool {
        let Some(block) =
            crate::block_navigation::block_at_or_under(&self.blocks, self.block_viewport_top())
        else {
            return false;
        };
        let id = block.id;
        let Some((start, end)) = self.block_anchor_range(block, false) else {
            return false;
        };
        let (start_point, end_point) = (self.anchor_point(start), self.anchor_point(end));
        self.term.selection = Some(Selection::new(
            SelectionType::Simple,
            start_point,
            Side::Left,
        ));
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(end_point, Side::Left);
        }
        self.selection_anchor = Some(start_point);
        self.selection_anchor_is_start = true;
        self.selection_kind = Some(SelectionType::Simple);
        self.selection_snapshot = self.term.selection_to_string();
        self.selected_block_id = Some(id);
        true
    }

    pub fn selected_block_output_text(&self) -> Option<String> {
        let id = self.selected_block_id?;
        let block = self.blocks.blocks().iter().find(|block| block.id == id)?;
        self.block_range_text(block, false)
    }

    pub fn selected_block_input_text(&self) -> Option<String> {
        let id = self.selected_block_id?;
        let block = self.blocks.blocks().iter().find(|block| block.id == id)?;
        self.block_range_text(block, true)
    }

    fn block_range_text(&self, block: &crate::blocks::Block, input: bool) -> Option<String> {
        let (start, end) = self.block_anchor_range(block, input)?;
        let (rows, wraps) = self.retained_grid_text();
        let retained_start = start.line.max(self.block_row_base() as i64);
        let first = retained_start.saturating_sub(self.block_row_base() as i64) as usize;
        let last = end.line.saturating_sub(self.block_row_base() as i64) as usize;
        if first >= rows.len() || last >= rows.len() || last < first {
            return None;
        }
        let start_column = if retained_start > start.line {
            0
        } else {
            start.column
        };
        crate::block_navigation::extract_range_text(
            &rows,
            &wraps,
            (first, start_column),
            (last, end.column),
        )
    }

    fn block_anchor_range(
        &self,
        block: &crate::blocks::Block,
        input: bool,
    ) -> Option<(crate::blocks::GridAnchor, crate::blocks::GridAnchor)> {
        if self.alternate_screen_active() {
            return None;
        }
        let (start, end) = if input {
            (
                block.grid_anchors.input_start?,
                block.grid_anchors.output_start?,
            )
        } else {
            (block.grid_anchors.output_start?, block.grid_anchors.end?)
        };
        (start.screen == crate::blocks::GridScreen::Primary
            && end.screen == crate::blocks::GridScreen::Primary)
            .then_some((start, end))
    }

    fn anchor_point(&self, anchor: crate::blocks::GridAnchor) -> Point {
        let history = self.term.grid().history_size() as i64;
        Point::new(
            Line(
                (anchor.line - self.stable_row_base as i64 - self.evicted_rows as i64 - history)
                    .clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            ),
            Column(anchor.column.min(self.term.columns().saturating_sub(1))),
        )
    }

    fn retained_grid_text(&self) -> (Vec<Vec<char>>, Vec<bool>) {
        let grid = self.term.grid();
        let history = grid.history_size();
        let mut rows = Vec::with_capacity(history + grid.screen_lines());
        let mut wraps = Vec::with_capacity(history + grid.screen_lines());
        for row in 0..history + grid.screen_lines() {
            let line = Line(row as i32 - history as i32);
            let cells = (0..grid.columns())
                .map(|column| grid[Point::new(line, Column(column))].c)
                .collect();
            let wrapped = grid[Point::new(line, Column(grid.columns() - 1))]
                .flags
                .contains(Flags::WRAPLINE);
            rows.push(cells);
            wraps.push(wrapped);
        }
        (rows, wraps)
    }
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }
    pub fn visible_top_line(&self) -> i64 {
        self.block_viewport_top()
    }
    fn visible_grid_top_line(&self) -> i32 {
        (self.visible_top_line()
            - (self.stable_row_base + self.evicted_rows + self.term.grid().history_size() as u64)
                as i64)
            .clamp(i32::MIN as i64, i32::MAX as i64) as i32
    }
    fn selection_point(&self, column: usize, row: usize) -> Point {
        Point::new(
            Line(self.visible_grid_top_line() + row as i32),
            Column(column.min(self.term.grid().columns() - 1)),
        )
    }
    pub fn begin_selection(&mut self, column: usize, row: usize, kind: SelectionType) {
        let point = self.selection_point(column, row);
        self.selection_anchor = Some(point);
        self.selection_anchor_is_start = true;
        self.selection_kind = Some(kind);
        self.selected_block_id = None;
        self.term.selection = Some(Selection::new(kind, point, Side::Left));
        self.term.reset_damage();
        self.selection_snapshot = self.term.selection_to_string();
    }
    pub fn extend_selection(&mut self, column: usize, row: usize) {
        let point = self.selection_point(column, row);
        if let (Some(anchor), Some(kind)) = (self.selection_anchor, self.selection_kind) {
            self.selection_anchor_is_start = anchor <= point;
            let (anchor_side, point_side) = if point < anchor {
                (Side::Right, Side::Left)
            } else {
                (Side::Left, Side::Right)
            };
            let mut selection = Selection::new(kind, anchor, anchor_side);
            selection.update(point, point_side);
            self.term.selection = Some(selection);
            self.selection_snapshot = self.term.selection_to_string();
        }
    }
    pub fn clear_selection(&mut self) {
        self.term.selection = None;
        self.selection_anchor = None;
        self.selection_anchor_is_start = true;
        self.selection_kind = None;
        self.selection_snapshot = None;
        self.selected_block_id = None;
    }
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string()
    }
    pub fn cell_selected(&self, column: usize, row: usize) -> bool {
        self.term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term))
            .is_some_and(|range| range.contains(self.selection_point(column, row)))
    }
    pub fn selected_cells(&self) -> Vec<(usize, usize)> {
        let grid = self.term.grid();
        let Some(range) = self
            .term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term))
        else {
            return Vec::new();
        };
        let top = self.visible_grid_top_line();
        let mut selected = Vec::new();
        for row in 0..grid.screen_lines() {
            let line = Line(top + row as i32);
            for column in 0..grid.columns() {
                if range.contains(Point::new(line, Column(column))) {
                    selected.push((column, row));
                }
            }
        }
        selected
    }
    pub fn scroll_up(&mut self, lines: usize) {
        if let Some(top) = self.viewport_top_line {
            self.viewport_top_line = Some(
                top.saturating_sub(lines.min(i64::MAX as usize) as i64)
                    .max(self.block_row_base() as i64),
            );
        } else {
            self.term
                .scroll_display(Scroll::Delta(lines.min(i32::MAX as usize) as i32));
        }
    }
    pub fn scroll_down(&mut self, lines: usize) {
        if let Some(top) = self.viewport_top_line {
            let max = self.block_row_base() as i64
                + self.term.grid().history_size() as i64
                + self.term.screen_lines() as i64
                - 1;
            self.viewport_top_line = Some(
                top.saturating_add(lines.min(i64::MAX as usize) as i64)
                    .min(max),
            );
        } else {
            self.term
                .scroll_display(Scroll::Delta(-(lines.min(i32::MAX as usize) as i32)));
        }
    }
    pub fn cursor(&self) -> Cursor {
        let p = self.term.grid().cursor.point;
        let row = p.line.0 - self.visible_grid_top_line();
        Cursor {
            column: p.column.0,
            row: if (0..self.term.screen_lines() as i32).contains(&row) {
                row as usize
            } else {
                usize::MAX
            },
            shape: self.term.cursor_style().shape,
        }
    }
    pub fn visible_cells(&self) -> Vec<Vec<VisibleCell>> {
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let top = self.visible_grid_top_line();
        let blank = Cell::default();
        (0..grid.screen_lines())
            .map(|row| {
                let line = Line(top + row as i32);
                (0..grid.columns())
                    .map(|col| {
                        if line.0 < -history || line.0 >= grid.screen_lines() as i32 {
                            visible(&blank)
                        } else {
                            visible(&grid[Point::new(line, Column(col))])
                        }
                    })
                    .collect()
            })
            .collect()
    }
    pub fn take_events(&mut self) -> Vec<ModelEvent> {
        self.listener
            .0
            .borrow_mut()
            .drain(..)
            .map(|event| match event {
                TermEvent::Title(v) => ModelEvent::Title(v),
                TermEvent::ResetTitle => ModelEvent::ResetTitle,
                TermEvent::PtyWrite(v) => ModelEvent::PtyWrite(v.into_bytes()),
                TermEvent::Bell => ModelEvent::Bell,
                other => ModelEvent::Other(format!("{other:?}")),
            })
            .collect()
    }
}

fn damage_intersects_selection(
    damage: LineDamageBounds,
    display_offset: usize,
    selection: SelectionRange,
) -> bool {
    let line = Line(damage.line as i32 - display_offset as i32);
    if line < selection.start.line || line > selection.end.line {
        return false;
    }
    let left = if selection.is_block || line == selection.start.line {
        selection.start.column.0
    } else {
        0
    };
    let right = if selection.is_block || line == selection.end.line {
        selection.end.column.0
    } else {
        usize::MAX
    };
    damage.left <= right && damage.right >= left
}

fn contains_non_osc_terminal_data(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"\x1b]") {
            index += 2;
            let mut terminated = false;
            while index < bytes.len() {
                if matches!(bytes[index], b'\x07' | b'\x18' | b'\x1a') {
                    index += 1;
                    terminated = true;
                    break;
                }
                if bytes[index] == b'\x1b' {
                    if bytes[index..].starts_with(b"\x1b\\") {
                        index += 2;
                        terminated = true;
                        break;
                    }
                    // VTE aborts this OSC; retain the ESC as the start of
                    // the next terminal sequence.
                    terminated = true;
                    break;
                }
                index += 1;
            }
            if !terminated {
                return false;
            }
        } else {
            return true;
        }
    }
    false
}

fn selection_outside_viewport(
    selection: SelectionRange,
    display_offset: usize,
    screen_lines: usize,
) -> bool {
    let first_visible_line = Line(-(display_offset as i32));
    let last_visible_line = Line(screen_lines as i32 - 1 - display_offset as i32);
    selection.start.line < first_visible_line || selection.end.line > last_visible_line
}

fn visible(cell: &Cell) -> VisibleCell {
    VisibleCell {
        character: cell.c,
        foreground: color(cell.fg),
        background: color(cell.bg),
        bold: cell.flags.contains(Flags::BOLD),
        inverse: cell.flags.contains(Flags::INVERSE),
    }
}
fn color(value: vte::ansi::Color) -> CellColor {
    match value {
        vte::ansi::Color::Indexed(i) => CellColor::Indexed(i),
        vte::ansi::Color::Spec(Rgb { r, g, b }) => CellColor::Rgb(r, g, b),
        vte::ansi::Color::Named(n) => CellColor::Named(n as u16),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::selection::SelectionType;
    use std::{
        thread,
        time::{Duration, Instant},
    };
    fn text(model: &Model) -> String {
        model
            .visible_cells()
            .iter()
            .map(|r| r.iter().map(|c| c.character).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn nonblank_text(model: &Model) -> String {
        model
            .visible_cells()
            .iter()
            .flatten()
            .map(|cell| cell.character)
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    fn primary(line: i64, column: usize) -> crate::blocks::GridAnchor {
        crate::blocks::GridAnchor {
            line,
            column,
            screen: crate::blocks::GridScreen::Primary,
        }
    }

    #[test]
    fn offscreen_selection_checks_skip_osc_only_segments() {
        assert!(!contains_non_osc_terminal_data(b"\x1b]133;A\x1b\\"));
        assert!(!contains_non_osc_terminal_data(
            b"\x1b]7;file://host/tmp\x1b\\"
        ));
        assert!(!contains_non_osc_terminal_data(b"\x1b]133;A\x18"));
        assert!(contains_non_osc_terminal_data(b"\x1b]bad\x18text"));
        assert!(contains_non_osc_terminal_data(b"output"));
        assert!(contains_non_osc_terminal_data(b"\x1b[2J"));
    }

    #[test]
    fn feed_updates_the_owned_block_list_from_observed_shell_marks() {
        let mut model = Model::new(20, 2);
        model.feed(
            b"\x1b]7;file://host/tmp\x1b\\\x1b]133;A\x1b\\\x1b]133;B\x1b\\\x1b]133;C\x1b\\\x1b]133;D;4\x1b\\",
        );
        let blocks = model.blocks().blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].state, crate::blocks::BlockState::Finished);
        assert_eq!(blocks[0].exit_status, Some(4));
        assert_eq!(blocks[0].cwd.as_ref().unwrap().path, b"/tmp");
    }

    #[test]
    fn marks_in_one_chunk_capture_cursor_positions_at_each_boundary() {
        let mut model = Model::new(20, 2);
        model.feed(b"\x1b]133;A\x1b\\p> \x1b]133;B\x1b\\abc\x1b]133;C\x1b\\\x1b]133;D;0\x1b\\");
        let block = &model.blocks().blocks()[0];
        assert_eq!(block.grid_anchors.prompt_start, Some(primary(0, 0)));
        assert_eq!(block.grid_anchors.input_start, Some(primary(0, 3)));
        assert_eq!(block.grid_anchors.output_start, Some(primary(0, 6)));
        assert_eq!(block.grid_anchors.end, Some(primary(0, 6)));
    }

    #[test]
    fn split_osc_completion_uses_absolute_offset_but_current_chunk_cursor() {
        let mut model = Model::new(20, 2);
        model.feed(b"abc\x1b]133;A");
        assert!(model.blocks().blocks().is_empty());
        model.feed(b"\x1b\\xy\x1b]133;B\x1b\\");
        let block = &model.blocks().blocks()[0];
        assert_eq!(block.boundaries.prompt_start, Some(12));
        assert_eq!(block.grid_anchors.prompt_start, Some(primary(0, 3)));
        assert_eq!(block.grid_anchors.input_start, Some(primary(0, 5)));
    }

    #[test]
    fn wrapped_mark_anchor_uses_the_wrapped_grid_row() {
        let mut model = Model::new(4, 2);
        model.feed(b"\x1b]133;A\x1b\\abcde\x1b]133;C\x1b\\");
        assert_eq!(
            model.blocks().blocks()[0].grid_anchors.output_start,
            Some(primary(1, 1))
        );
    }

    #[test]
    fn anchor_row_stays_stable_as_scrollback_grows_without_eviction() {
        let mut model = Model::new(8, 2);
        model.feed(b"\x1b]133;A\x1b\\");
        let initial = model.blocks().blocks()[0].grid_anchors.prompt_start;
        assert_eq!(initial, Some(primary(0, 0)));
        model.feed(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        model.scroll_up(2);
        assert_eq!(
            model.blocks().blocks()[0].grid_anchors.prompt_start,
            initial
        );
    }

    #[test]
    fn resize_invalidates_anchors_that_may_have_reflowed() {
        let mut model = Model::new(8, 2);
        model.feed(b"\x1b]133;A\x1b\\");
        assert!(
            model.blocks().blocks()[0]
                .grid_anchors
                .prompt_start
                .is_some()
        );
        model.resize(3, 4);
        assert_eq!(model.blocks().blocks()[0].grid_anchors.prompt_start, None);

        let mut wider = Model::new(3, 4);
        wider.feed(b"\x1b]133;A\x1b\\");
        wider.resize(8, 2);
        assert_eq!(wider.blocks().blocks()[0].grid_anchors.prompt_start, None);
    }

    #[test]
    fn alternate_screen_marks_are_not_anchored_to_primary_rows() {
        let mut model = Model::new(8, 2);
        model.feed(b"\x1b[?1049h\x1b]133;A\x1b\\");
        let anchor = model.blocks().blocks()[0].grid_anchors.prompt_start;
        assert_eq!(
            anchor.unwrap().screen,
            crate::blocks::GridScreen::Alternate,
            "alternate-screen rows must not masquerade as primary rows"
        );
    }

    #[test]
    fn completed_blocks_with_evicted_rows_are_expired() {
        let mut model = Model::with_scrollback_limit(20, 2, 4);
        model.feed(b"\x1b]133;A\x1b\\\x1b]133;C\x1b\\\x1b]133;D;0\x1b\\");
        assert!(
            model.blocks().blocks()[0]
                .grid_anchors
                .prompt_start
                .is_some()
        );
        let lines = b"x\r\n".repeat(20);
        model.feed(&lines);
        assert!(model.blocks().blocks().is_empty());
    }

    #[test]
    fn full_scrollback_evicts_only_anchors_older_than_the_exact_boundary() {
        let mut model = Model::with_scrollback_limit(20, 2, 2);
        model.feed(b"\x1b]133;A\x1b\\\x1b]133;C\x1b\\\x1b]133;D;0\x1b\\");
        model.feed(b"one\r\ntwo\r\nthree\r\n");
        assert_eq!(model.evicted_rows, 0, "history growth is not eviction");
        model.feed(b"\x1b]133;A\x1b\\\x1b]133;C\x1b\\\x1b]133;D;0\x1b\\");
        assert_eq!(model.blocks().blocks().len(), 2);
        let newer_anchor = model
            .blocks()
            .blocks()
            .last()
            .unwrap()
            .grid_anchors
            .prompt_start;
        assert!(newer_anchor.is_some());

        model.feed(b"four\r\n");

        let blocks = model.blocks().blocks();
        assert_eq!(
            model.evicted_rows, 1,
            "one full-screen scroll evicts one row"
        );
        assert_eq!(
            blocks.len(),
            1,
            "only the actually evicted block should be pruned"
        );
        assert_eq!(blocks[0].grid_anchors.prompt_start, newer_anchor);
    }

    #[test]
    fn partial_scroll_region_never_advances_primary_eviction_boundary() {
        let mut model = Model::with_scrollback_limit(20, 3, 1);
        model.feed(b"\x1b]133;A\x1b\\\x1b]133;C\x1b\\\x1b]133;D;0\x1b\\");
        model.feed(b"one\r\ntwo\r\nthree\r\n");
        model.feed(b"\x1b]133;A\x1b\\");
        let anchor = model
            .blocks()
            .blocks()
            .last()
            .unwrap()
            .grid_anchors
            .prompt_start;
        let evicted_before = model.evicted_rows;
        model.feed(b"\x1b[2;3r\x1b[3;1Hpartial\r\n");
        assert_eq!(model.evicted_rows, evicted_before);
        assert_eq!(
            model
                .blocks()
                .blocks()
                .last()
                .unwrap()
                .grid_anchors
                .prompt_start,
            anchor,
            "scrolling inside a partial DECSTBM region does not evict history rows"
        );
    }

    #[test]
    fn wrapped_printable_input_counts_only_rows_evicted_after_history_fills() {
        let mut model = Model::with_scrollback_limit(2, 2, 2);
        model.feed(b"abcde");
        assert_eq!(model.term.grid().history_size(), 1);
        assert_eq!(model.evicted_rows, 0);
        model.feed(b"fg");
        assert_eq!(model.term.grid().history_size(), 2);
        assert_eq!(model.evicted_rows, 0);
        model.feed(b"hi");
        assert_eq!(model.term.grid().history_size(), 2);
        assert_eq!(model.evicted_rows, 1);
    }

    #[test]
    fn wide_character_wrap_counts_the_actual_full_history_scroll() {
        let mut model = Model::with_scrollback_limit(2, 2, 1);
        model.feed("abc日".as_bytes());
        assert_eq!(model.term.grid().history_size(), 1);
        assert_eq!(model.evicted_rows, 0);
        model.feed("本".as_bytes());
        assert_eq!(model.term.grid().history_size(), 1);
        assert_eq!(model.evicted_rows, 1);
    }

    #[test]
    fn saved_history_clear_advances_eviction_boundary_and_expires_old_rows() {
        let mut model = Model::with_scrollback_limit(20, 2, 2);
        model.feed(b"\x1b]133;A\x1b\\");
        model.feed(b"one\r\ntwo\r\nthree\r\n");
        assert_eq!(model.term.grid().history_size(), 2);
        let before = model.evicted_rows;
        model.feed(b"\x1b[3J");
        assert_eq!(model.term.grid().history_size(), 0);
        assert_eq!(model.evicted_rows, before + 2);
        assert_eq!(model.blocks().blocks()[0].grid_anchors.prompt_start, None);
    }

    #[test]
    fn clear_all_and_terminal_reset_invalidate_primary_anchors() {
        for control in [b"\x1b[2J".as_slice(), b"\x1bc".as_slice()] {
            let mut model = Model::with_scrollback_limit(20, 2, 2);
            model.feed(b"\x1b]133;A\x1b\\");
            model.feed(control);
            assert_eq!(
                model.blocks().blocks()[0].grid_anchors.prompt_start,
                None,
                "control sequence {control:?} clears the grid anchor"
            );
        }
    }

    #[test]
    fn clear_all_and_reset_keep_new_anchor_coordinates_monotonic() {
        for control in [b"\x1b[2J".as_slice(), b"\x1bc".as_slice()] {
            let mut model = Model::new(20, 3);
            model.feed(b"old\r\nrows\r\nmore\r\nlast\r\n");
            model.feed(b"\x1b]133;A\x1b\\");
            let old = model
                .blocks()
                .blocks()
                .last()
                .unwrap()
                .grid_anchors
                .prompt_start
                .unwrap()
                .line;
            model.feed(control);
            model.feed(b"\x1b]133;A\x1b\\");
            let new = model
                .blocks()
                .blocks()
                .last()
                .unwrap()
                .grid_anchors
                .prompt_start
                .unwrap()
                .line;
            assert!(new > old, "{control:?}: new={new}, old={old}");
        }
    }

    #[test]
    fn clear_all_epoch_anchor_projects_in_the_current_viewport() {
        let mut model = Model::new(20, 3);
        model.feed(b"\x1b]133;A\x1b\\\x1b[2J\x1b]133;A\x1b\\");
        let viewport = crate::renderer::BlockViewport {
            evicted_rows: model.block_row_base(),
            history_size: model.history_size(),
            display_offset: model.display_offset(),
            first_visible_line: Some(model.visible_top_line()),
            screen_lines: 3,
            alternate_screen: false,
        };
        let projected = crate::renderer::project_block_decorations(model.blocks(), viewport);
        assert_eq!(projected.len(), 1);
        assert_eq!(
            projected[0].block_id,
            model.blocks().blocks().last().unwrap().id
        );
        assert_eq!(projected[0].separator_row, Some(0));
    }

    #[test]
    fn wide_wrap_prediction_matches_terminal_for_odd_and_even_widths() {
        for cols in [4, 5] {
            let mut model = Model::with_scrollback_limit(cols, 2, 0);
            model.feed(b"top\r\nbot");
            model.feed(format!("\x1b[2;{cols}H日").as_bytes());
            assert_eq!(
                model.evicted_rows, 0,
                "a wide glyph shifted left fits; columns={cols}"
            );

            let mut pending = Model::with_scrollback_limit(cols, 2, 0);
            pending.feed(b"top\r\nbot");
            let fill = if cols == 4 {
                b"X".as_slice()
            } else {
                b"XY".as_slice()
            };
            pending.feed(fill);
            assert!(
                pending.term.grid().cursor.input_needs_wrap,
                "columns={cols}"
            );
            pending.feed("日".as_bytes());
            assert_eq!(pending.evicted_rows, 1, "columns={cols}");
        }
    }

    #[test]
    fn block_selection_and_copy_ranges_exclude_the_prompt_and_preserve_wrapped_text() {
        let mut model = Model::new(12, 3);
        model.feed(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\echo hi\r\n\x1b]133;C\x1b\\a\r\nb\x1b]133;D;0\x1b\\");
        let block = model.blocks().blocks().last().unwrap();
        assert_eq!(
            model.block_range_text(block, true).as_deref(),
            Some("echo hi")
        );
        assert_eq!(
            model.block_range_text(block, false).as_deref(),
            Some("a\nb")
        );
        assert!(model.select_block_at_viewport());
        assert_eq!(model.selected_block_output_text().as_deref(), Some("a\nb"));
        assert!(model.cell_selected(0, 1));
    }

    #[test]
    fn block_copy_refuses_when_its_output_start_was_evicted_or_is_alternate() {
        let mut model = Model::with_scrollback_limit(12, 2, 1);
        model.feed(b"\x1b]133;A\x1b\\\x1b]133;B\x1b\\\x1b]133;C\x1b\\old\r\nline\r\nnew\r\nlast\x1b]133;D;0\x1b\\");
        let block = model.blocks().blocks().last().unwrap();
        assert_eq!(block.grid_anchors.output_start, None);
        assert_eq!(model.block_range_text(block, false), None);

        let mut alternate = Model::new(12, 2);
        alternate.feed(b"\x1b[?1049h\x1b]133;A\x1b\\\x1b]133;C\x1b\\output\x1b]133;D;0\x1b\\");
        let block = alternate.blocks().blocks().last().unwrap();
        assert_eq!(alternate.block_range_text(block, false), None);
    }

    #[test]
    fn navigation_aligns_a_recent_prompt_to_the_top_of_the_viewport() {
        let mut model = Model::new(20, 3);
        model.feed(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\echo one\r\n\x1b]133;C\x1b\\one\r\n\x1b]133;D;0\x1b\\\x1b]133;A\x1b\\$ ");
        let second_prompt = model
            .blocks()
            .blocks()
            .last()
            .unwrap()
            .grid_anchors
            .prompt_start
            .unwrap()
            .line;
        model.navigate_block(crate::block_navigation::NavigationDirection::Next);
        assert_eq!(model.block_viewport_top(), second_prompt);
        assert!(text(&model).lines().next().unwrap().starts_with("$ "));
    }

    #[test]
    fn terminal_reset_invalidates_primary_block_anchors() {
        let mut model = Model::with_scrollback_limit(20, 2, 2);
        model.feed(b"\x1b]133;A\x1b\\");
        assert!(
            model.blocks().blocks()[0]
                .grid_anchors
                .prompt_start
                .is_some()
        );
        model.feed(b"\x1bc");
        assert_eq!(model.blocks().blocks()[0].grid_anchors.prompt_start, None);
    }

    #[test]
    fn eviction_after_resize_keeps_blocks_whose_anchors_were_invalidated() {
        let mut model = Model::with_scrollback_limit(20, 2, 1);
        model.feed(b"\x1b]133;A\x1b\\\x1b]133;C\x1b\\\x1b]133;D;0\x1b\\");
        model.resize(20, 3);
        assert_eq!(model.blocks().blocks()[0].grid_anchors.prompt_start, None);
        model.feed(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        assert_eq!(model.evicted_rows, 1);
        assert_eq!(model.blocks().blocks().len(), 1);
        assert_eq!(
            model.blocks().blocks()[0].state,
            crate::blocks::BlockState::Finished
        );
        assert!(
            model.blocks().blocks()[0]
                .grid_anchors
                .prompt_start
                .is_none()
        );
    }

    #[test]
    fn wide_glyph_shifted_left_at_the_last_column_does_not_fake_an_eviction() {
        let mut model = Model::with_scrollback_limit(20, 2, 1);
        model.feed(b"one\r\ntwo\r\nthree\r\n");
        assert_eq!(model.term.grid().history_size(), 1);
        assert_eq!(model.evicted_rows, 1);
        model.feed(b"\x1b[2;20H\xe6\x97\xa5");
        assert_eq!(model.term.grid().history_size(), 1);
        assert_eq!(model.evicted_rows, 1);
    }

    #[test]
    fn text_sgr_cursor_and_clear_are_emulated() {
        let mut m = Model::new(10, 3);
        m.feed(b"hello\x1b[31;44;1mX\x1b[7mY\x1b[0m\x1b[2;3H!");
        let g = m.visible_cells();
        assert_eq!(&text(&m)[..5], "hello");
        assert!(g[0][5].bold);
        assert_eq!(g[0][5].foreground, CellColor::Named(1));
        assert_eq!(g[0][5].background, CellColor::Named(4));
        assert!(g[0][6].inverse);
        assert_eq!(m.cursor().column, 3);
        assert_eq!(m.cursor().row, 1);
        m.feed(b"\x1b[2J\x1b[H");
        assert!(text(&m).chars().all(|c| c == ' ' || c == '\n'));
    }
    #[test]
    fn wraps_scrolls_and_resizes_preserving_renderable_grid() {
        let mut m = Model::new(3, 2);
        m.feed(b"abcdefghi");
        assert!(text(&m).contains("ghi"));
        m.scroll_up(2);
        assert!(m.display_offset() > 0);
        let before = m.visible_cells();
        m.resize(4, 3);
        assert_eq!(m.visible_cells().len(), 3);
        assert_eq!(m.visible_cells()[0].len(), 4);
        assert!(!before.is_empty());
        assert_eq!(
            nonblank_text(&m),
            "abcdefghi",
            "resizing must reflow, not discard, wrapped text"
        );
        let offset = m.display_offset();
        m.scroll_up(2);
        assert!(m.display_offset() >= offset);
        m.scroll_down(100);
        assert_eq!(m.display_offset(), 0);
    }
    #[test]
    fn scroll_to_bottom_uses_terminal_bottom_position() {
        let mut model = Model::new(10, 2);
        model.feed(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        model.scroll_up(100);
        assert!(model.display_offset() > 0);
        model.scroll_to_bottom();
        assert_eq!(model.display_offset(), 0);
    }

    #[test]
    fn terminal_input_modes_follow_application_escape_sequences() {
        let mut m = Model::new(10, 2);
        assert!(!m.application_cursor_keys());
        assert!(!m.bracketed_paste_enabled());
        assert!(!m.mouse_or_alternate_screen_active());
        m.feed(b"\x1b[?1h\x1b[?2004h\x1b[?1000h");
        assert!(m.application_cursor_keys());
        assert!(m.bracketed_paste_enabled());
        assert!(m.mouse_or_alternate_screen_active());
        m.feed(b"\x1b[?1l\x1b[?2004l\x1b[?1000l");
        assert!(!m.application_cursor_keys());
        assert!(!m.bracketed_paste_enabled());
        assert!(!m.mouse_or_alternate_screen_active());
    }

    #[test]
    fn alacritty_selection_extracts_words_lines_and_highlights_cells() {
        let mut model = Model::new(20, 3);
        model.feed(b"alpha beta\r\nsecond line");
        model.begin_selection(7, 0, SelectionType::Semantic);
        assert_eq!(model.selection_text().as_deref(), Some("beta"));
        assert!(model.cell_selected(7, 0));
        assert!(!model.cell_selected(2, 0));
        model.begin_selection(3, 0, SelectionType::Lines);
        assert_eq!(model.selection_text().as_deref(), Some("alpha beta\n"));
        model.feed(b"!");
        assert_eq!(model.selection_text().as_deref(), Some("alpha beta\n"));
    }

    #[test]
    fn output_overwriting_selected_text_clears_selection() {
        let mut model = Model::new(20, 2);
        model.feed(b"alpha beta");
        model.begin_selection(0, 0, SelectionType::Simple);
        model.extend_selection(4, 0);
        assert_eq!(model.selection_text().as_deref(), Some("alpha"));
        model.feed(b"\x1b[1;1H!\x1b[K");
        assert!(model.term.selection.is_none());
    }

    #[test]
    fn dragged_selection_tracks_scrollback_cells() {
        let mut model = Model::new(12, 2);
        model.feed(b"first\r\nsecond\r\nthird\r\n");
        model.scroll_up(100);
        model.begin_selection(0, 0, SelectionType::Simple);
        model.extend_selection(4, 0);
        assert_eq!(model.selection_text().as_deref(), Some("first"));
        assert!(model.cell_selected(4, 0));
        assert!(!model.cell_selected(1, 1));
    }

    #[test]
    fn simple_selection_includes_both_endpoints_in_either_drag_direction() {
        let mut model = Model::new(20, 2);
        model.feed(b"abcdef");
        model.begin_selection(1, 0, SelectionType::Simple);
        model.extend_selection(4, 0);
        assert_eq!(model.selection_text().as_deref(), Some("bcde"));
        model.begin_selection(4, 0, SelectionType::Simple);
        model.extend_selection(1, 0);
        assert_eq!(model.selection_text().as_deref(), Some("bcde"));
        assert!(model.cell_selected(1, 0));
        assert!(model.cell_selected(4, 0));
        assert_eq!(model.selected_cells(), [(1, 0), (2, 0), (3, 0), (4, 0)]);
    }

    #[test]
    fn selection_damage_intersection_uses_visible_columns_and_rows() {
        let range = SelectionRange::new(
            Point::new(Line(2), Column(3)),
            Point::new(Line(3), Column(5)),
            false,
        );
        assert!(damage_intersects_selection(
            LineDamageBounds::new(2, 3, 4),
            0,
            range
        ));
        assert!(!damage_intersects_selection(
            LineDamageBounds::new(2, 0, 2),
            0,
            range
        ));
        assert!(damage_intersects_selection(
            LineDamageBounds::new(3, 0, 0),
            0,
            range
        ));
        assert!(!damage_intersects_selection(
            LineDamageBounds::new(4, 3, 4),
            0,
            range
        ));
    }

    #[test]
    fn selection_invalidates_when_its_live_line_changes_offscreen() {
        let mut model = Model::new(12, 2);
        model.feed(b"alpha\r\nbeta\r\ngamma\r\ndelta");
        model.begin_selection(0, 0, SelectionType::Simple);
        model.extend_selection(4, 0);
        assert_eq!(model.selection_text().as_deref(), Some("gamma"));

        model.scroll_up(2);
        assert!(model.display_offset() > 0);
        assert_eq!(model.selection_text().as_deref(), Some("gamma"));
        model.term.reset_damage();
        model.feed(b"\x1b[1;1HZZ");

        assert!(model.term.selection.is_none());
    }

    #[test]
    fn selection_anchor_tracks_its_line_when_output_scrolls_the_grid() {
        let mut model = Model::new(12, 2);
        model.feed(b"alpha\r\nbeta");
        model.begin_selection(0, 0, SelectionType::Simple);
        model.extend_selection(4, 0);
        assert_eq!(model.selection_text().as_deref(), Some("alpha"));

        model.feed(b"\r\ngamma");
        model.extend_selection(4, 1);

        assert_eq!(
            model.selection_text().as_deref(),
            Some("alpha\nbeta\ngamma")
        );
    }

    #[test]
    fn backward_selection_anchor_rebases_when_output_scrolls_the_grid() {
        let mut model = Model::new(12, 2);
        model.feed(b"alpha\r\nbeta");
        model.begin_selection(4, 1, SelectionType::Simple);
        model.extend_selection(0, 0);
        assert_eq!(model.selection_text().as_deref(), Some("alpha\nbeta"));

        model.feed(b"\r\ngamma");
        assert_eq!(model.selection_text().as_deref(), Some("alpha\nbeta"));
        let range = model
            .term
            .selection
            .as_ref()
            .unwrap()
            .to_range(&model.term)
            .unwrap();
        assert!(!model.selection_anchor_is_start);
        assert_eq!(model.selection_anchor.unwrap().line, range.end.line);
    }
    #[test]
    fn cursor_shape_osc_and_dsr_writeback_are_exposed() {
        let mut m = Model::new(10, 2);
        let events = m.feed(b"\x1b[4 q\x1b[6n\x1b]133;A\x07\x1b]0;model-title\x07\x07");
        assert_eq!(m.cursor().shape, vte::ansi::CursorShape::Underline);
        assert!(events.iter().any(|e| matches!(
            e.event,
            crate::osc::Event::SemanticMark(crate::osc::SemanticMark::PromptStart)
        )));
        let terminal_events = m.take_events();
        assert!(terminal_events.contains(&ModelEvent::PtyWrite(b"\x1b[1;1R".to_vec())));
        assert!(terminal_events.contains(&ModelEvent::Title("model-title".into())));
        assert!(terminal_events.contains(&ModelEvent::Bell));
    }
    #[test]
    fn line_wrap_and_many_lines_retain_scrollback() {
        let mut m = Model::new(4, 2);
        m.feed(b"OLD-LINE\r\nNEXT-ONE\r\nNEXT-TWO\r\nNEXT-END\r\n");
        m.scroll_up(1000);
        assert!(m.display_offset() > 0);
        assert_eq!(
            nonblank_text(&m),
            "OLD-LINE",
            "scrolled snapshot should show the known older line"
        );
    }
    #[cfg(unix)]
    #[test]
    fn renders_prompt_bytes_from_built_so1sh_pty() {
        let executable = crate::pty::test_so1sh_executable();
        let executable = executable.expect(
            "so1sh binary required for real PTY prompt test; run `cargo build -p so1sh` first",
        );
        let mut pty = crate::pty::Pty::spawn(&executable, "/", 80, 24).expect("spawn built so1sh");
        let mut model = Model::new(80, 24);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while let Some(chunk) = pty.try_read() {
                model.feed(&chunk);
            }
            let screen = text(&model);
            if screen.contains("$ ") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "so1sh prompt not rendered; screen: {screen:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        pty.write(b"exit\n").unwrap();
    }
}

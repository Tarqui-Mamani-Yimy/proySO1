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
    /// Current DECSTBM region, tracked because Term does not expose it.
    scroll_region: std::ops::Range<usize>,
    selection_anchor: Option<Point>,
    selection_anchor_is_start: bool,
    selection_kind: Option<SelectionType>,
    selection_snapshot: Option<String>,
}

/// VTE handler proxy that delegates every operation to Term while observing
/// only operations which can push primary rows out of its bounded history.
struct EvictionTracker<'a> {
    term: &'a mut Term<Listener>,
    evicted_rows: &'a mut u64,
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
}

impl vte::ansi::Handler for EvictionTracker<'_> {
    fn input(&mut self, character: char) {
        let cursor = &self.term.grid().cursor;
        let pending_wrap = cursor.input_needs_wrap;
        let at_last_column = cursor.point.column.0 + 1 == self.term.columns();
        let bottom_row = cursor.point.line.0 + 1 == self.region.end as i32;
        let can_wrap = self.is_full_primary_region()
            && bottom_row
            && self
                .term
                .mode()
                .contains(alacritty_terminal::term::TermMode::LINE_WRAP);
        let candidate_line = cursor.point.line;
        let candidate_column = cursor.point.column;
        let history_before = self.term.grid().history_size();
        self.term.input(character);
        let wide_char_wrapped = can_wrap
            && at_last_column
            && self.term.grid()[candidate_line][candidate_column]
                .flags
                .contains(Flags::LEADING_WIDE_CHAR_SPACER);
        if can_wrap && (pending_wrap || wide_char_wrapped) {
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
        self.term.reset_state();
        self.region = 0..self.term.screen_lines();
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
        clear_screen(_mode: vte::ansi::ClearMode);
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
            scroll_region: 0..rows.max(1),
            selection_anchor: None,
            selection_anchor_is_start: true,
            selection_kind: None,
            selection_snapshot: None,
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<crate::osc::ObservedEvent> {
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
            evicted_rows: &mut self.evicted_rows,
            history_limit: self.scrollback_limit,
            region: self.scroll_region.clone(),
        };
        self.processor.advance(&mut tracker, bytes);
        self.scroll_region = tracker.region;
        if self.evicted_rows > evicted_before {
            self.blocks.expire_primary_anchors_before(self.evicted_rows);
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
                self.evicted_rows as i64 + grid.history_size() as i64 + point.line.0 as i64
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

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if self.term.columns() != cols || self.term.screen_lines() != rows {
            self.blocks.invalidate_anchors();
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
    }
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }
    fn selection_point(&self, column: usize, row: usize) -> Point {
        Point::new(
            Line(row as i32 - self.term.grid().display_offset() as i32),
            Column(column.min(self.term.grid().columns() - 1)),
        )
    }
    pub fn begin_selection(&mut self, column: usize, row: usize, kind: SelectionType) {
        let point = self.selection_point(column, row);
        self.selection_anchor = Some(point);
        self.selection_anchor_is_start = true;
        self.selection_kind = Some(kind);
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
        let offset = grid.display_offset() as i32;
        let mut selected = Vec::new();
        for row in 0..grid.screen_lines() {
            let line = Line(row as i32 - offset);
            for column in 0..grid.columns() {
                if range.contains(Point::new(line, Column(column))) {
                    selected.push((column, row));
                }
            }
        }
        selected
    }
    pub fn scroll_up(&mut self, lines: usize) {
        self.term
            .scroll_display(Scroll::Delta(lines.min(i32::MAX as usize) as i32));
    }
    pub fn scroll_down(&mut self, lines: usize) {
        self.term
            .scroll_display(Scroll::Delta(-(lines.min(i32::MAX as usize) as i32)));
    }
    pub fn cursor(&self) -> Cursor {
        let p = self.term.grid().cursor.point;
        Cursor {
            column: p.column.0,
            row: p.line.0.max(0) as usize,
            shape: self.term.cursor_style().shape,
        }
    }
    pub fn visible_cells(&self) -> Vec<Vec<VisibleCell>> {
        let grid = self.term.grid();
        (0..grid.screen_lines())
            .map(|row| {
                let line = Line(row as i32 - grid.display_offset() as i32);
                (0..grid.columns())
                    .map(|col| visible(&grid[Point::new(line, Column(col))]))
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

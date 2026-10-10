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
    selection_anchor: Option<Point>,
    selection_anchor_is_start: bool,
    selection_kind: Option<SelectionType>,
    selection_snapshot: Option<String>,
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
        let listener = Listener::default();
        let term = Term::new(
            Config::default(),
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
            selection_anchor: None,
            selection_anchor_is_start: true,
            selection_kind: None,
            selection_snapshot: None,
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<crate::osc::ObservedEvent> {
        let previous_range = self
            .term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term));
        let events = self.osc.feed(bytes);
        self.blocks.consume(&events);
        self.processor.advance(&mut self.term, bytes);
        if !bytes.is_empty() && self.term.selection.is_some() {
            let range = self
                .term
                .selection
                .as_ref()
                .and_then(|selection| selection.to_range(&self.term));
            let offset = self.term.grid().display_offset();
            let intersects_selection = match self.term.damage() {
                alacritty_terminal::term::TermDamage::Full => true,
                alacritty_terminal::term::TermDamage::Partial(damage) => {
                    range.is_some_and(|range| {
                        damage
                            .into_iter()
                            .any(|line| damage_intersects_selection(line, offset, range))
                    })
                }
            };
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
            let partially_offscreen = range.is_some_and(|range| {
                selection_outside_viewport(range, offset, self.term.grid().screen_lines())
            });
            self.term.reset_damage();
            if (intersects_selection || partially_offscreen)
                && self.term.selection_to_string() != self.selection_snapshot
            {
                self.clear_selection();
            }
        }
        events
    }

    pub fn blocks(&self) -> &crate::blocks::BlockList {
        &self.blocks
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.term.resize(Size {
            cols: cols.max(1),
            rows: rows.max(1),
        });
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

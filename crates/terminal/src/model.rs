//! Headless VT model adapter. The same byte stream is observed for shell OSCs
//! and consumed by Alacritty's persistent ANSI parser.
use std::cell::RefCell;
use std::rc::Rc;

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point},
    term::{
        Config, Term,
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
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<crate::osc::ObservedEvent> {
        let events = self.osc.feed(bytes);
        self.processor.advance(&mut self.term, bytes);
        events
    }
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.term.resize(Size {
            cols: cols.max(1),
            rows: rows.max(1),
        });
    }
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
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

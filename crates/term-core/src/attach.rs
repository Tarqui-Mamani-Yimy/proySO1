//! Attach protocol between a `so1term-server` session and one attached GUI.
//!
//! A control connection is upgraded to a bidirectional attach stream. Both
//! directions carry newline-delimited JSON: exactly one message per line, at
//! most [`MAX_ATTACH_LINE_BYTES`] including the newline. Every message is a
//! JSON object with `"v": 1` ([`ATTACH_PROTOCOL_VERSION`]) and a `"type"`
//! string; the remaining keys depend on the type. Parsing is strict: an
//! oversized line is rejected before JSON decoding, and an unknown type, a
//! wrong version, or a missing, extra, or ill-typed field is a typed
//! [`AttachError`]. Optional fields are omitted when absent; `null` is never a
//! valid value.
//!
//! The server is authoritative. It owns every pane's VT model and view state
//! (scroll position, selection, block navigation) and sends:
//! - [`ServerMessage::Layout`] with tab and pane geometry,
//! - [`ServerMessage::Frame`] with per-pane content (see [`PaneFrame`] for the
//!   sequence and resync rule),
//! - replies to copy and layout requests, errors, and the final exit reason,
//! - kitty graphics image data the frames' placements reference (see
//!   [`ServerMessage::Image`]).
//!
//! The client sends its window metrics first ([`ClientMessage::Hello`]), then
//! input bytes it already encoded with the frame's keyboard modes, view and
//! layout actions, explicit resync requests, and finally `Detach`.
//!
//! Frame lines are encoded as styled runs: `[text, fg, bg, flags]` where `text`
//! holds exactly one character per cell, colors are `#rrggbb`, `i<0-255>`
//! (indexed) or `n<u16>` (named), and `flags` has value 1 for bold and value 2
//! for inverse. [`line_runs`] and [`cells_from_runs`] convert between visible
//! cells and runs; [`selection_spans`] and [`selection_cells`] do the same for
//! selected cells.

use crate::model::{CellColor, Cursor, VisibleCell};
use crate::renderer::{BlockDecoration, BlockDecorationColor};
use alacritty_terminal::selection::SelectionType;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use std::fmt;
use vte::ansi::CursorShape;

pub const ATTACH_PROTOCOL_VERSION: u64 = 1;
/// Maximum attach line length in bytes, including the trailing newline.
pub const MAX_ATTACH_LINE_BYTES: usize = 16 * 1024 * 1024;
/// Most raw image bytes one [`ServerMessage::ImageData`] chunk carries.
pub const MAX_IMAGE_CHUNK_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachErrorCode {
    MalformedJson,
    LineTooLarge,
    UnknownVersion,
    UnknownType,
    MissingField,
    InvalidField,
}

impl AttachErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MalformedJson => "MALFORMED_JSON",
            Self::LineTooLarge => "LINE_TOO_LARGE",
            Self::UnknownVersion => "UNKNOWN_VERSION",
            Self::UnknownType => "UNKNOWN_TYPE",
            Self::MissingField => "MISSING_FIELD",
            Self::InvalidField => "INVALID_FIELD",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachError {
    pub code: AttachErrorCode,
    pub message: String,
}

impl AttachError {
    pub fn new(code: AttachErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for AttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for AttachError {}

type AttachResult<T> = Result<T, AttachError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionOp {
    Begin {
        column: usize,
        row: usize,
        kind: SelectionType,
    },
    Extend {
        column: usize,
        row: usize,
    },
    Clear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockOp {
    Previous,
    Next,
    Select,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopySource {
    Selection,
    BlockInput,
    BlockOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SplitDirection {
    Right,
    Down,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutAction {
    NewTab,
    CloseTab,
    NextTab,
    PreviousTab,
    SelectTab { index: usize },
    Split { direction: SplitDirection },
    ClosePane,
    FocusDirection { direction: FocusDirection },
    FocusPane { pane: u64 },
}

/// Messages sent by the attached GUI. Sizes are in pixels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientMessage {
    /// First message on an attach stream; the server adopts these metrics.
    Hello {
        width: u32,
        height: u32,
        cell_width: u32,
        cell_height: u32,
    },
    /// Bytes for the pane's PTY, already encoded by the client (base64 on the wire).
    Input {
        pane: u64,
        data: Vec<u8>,
    },
    Resize {
        width: u32,
        height: u32,
        cell_width: u32,
        cell_height: u32,
    },
    /// Positive `lines` scroll into history.
    Scroll {
        pane: u64,
        lines: i32,
    },
    ScrollToBottom {
        pane: u64,
    },
    Selection {
        pane: u64,
        op: SelectionOp,
    },
    Block {
        pane: u64,
        op: BlockOp,
    },
    Copy {
        request: u64,
        pane: u64,
        source: CopySource,
    },
    Layout {
        request: u64,
        action: LayoutAction,
    },
    /// Requests full frames for one pane, or for every pane when `pane` is absent.
    Resync {
        pane: Option<u64>,
    },
    Detach,
}

/// Cells sharing one style; `text` holds exactly one character per cell.
#[derive(Clone, Debug, PartialEq)]
pub struct StyledRun {
    pub text: String,
    pub foreground: CellColor,
    pub background: CellColor,
    pub bold: bool,
    pub inverse: bool,
}

impl StyledRun {
    pub fn same_style(&self, other: &Self) -> bool {
        self.foreground == other.foreground
            && self.background == other.background
            && self.bold == other.bold
            && self.inverse == other.inverse
    }

    fn matches_cell(&self, cell: &VisibleCell) -> bool {
        self.foreground == cell.foreground
            && self.background == cell.background
            && self.bold == cell.bold
            && self.inverse == cell.inverse
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FrameLine {
    pub row: usize,
    pub runs: Vec<StyledRun>,
}

/// Selected cells `start..=end` (inclusive columns) on one viewport row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionSpan {
    pub row: usize,
    pub start: usize,
    pub end: usize,
}

/// Input-relevant terminal modes the client needs to encode keys locally.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PaneModes {
    /// Raw [`crate::keyboard::KeyboardFlags`] bits of the active screen.
    pub keyboard_flags: u8,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse_or_alternate: bool,
    pub alternate_screen: bool,
}

/// One versioned view of a pane.
///
/// Sequence rule: per pane, `seq` starts at 1 and increments by one for every
/// frame the server sends for that pane. A `full` frame replaces the whole
/// pane view (every row not listed is blank); a delta frame only replaces the
/// listed rows, while cursor, selection, modes, decorations, title and cwd are
/// always complete. A client that receives a non-full frame whose `seq` is
/// not the last seen `seq + 1` sends `Resync { pane }` and ignores deltas for
/// that pane until the next full frame.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneFrame {
    pub pane: u64,
    pub seq: u64,
    pub full: bool,
    pub cols: usize,
    pub rows: usize,
    pub lines: Vec<FrameLine>,
    pub cursor: Cursor,
    pub selection: Vec<SelectionSpan>,
    pub modes: PaneModes,
    pub decorations: Vec<BlockDecoration>,
    /// Empty when the pane has no title.
    pub title: String,
    /// Empty when the pane has not reported a working directory.
    pub cwd: String,
    /// Visible kitty image placements in drawing order; always complete.
    pub placements: Vec<FramePlacement>,
}

/// One kitty image placement projected onto the pane viewport.
///
/// `column`/`row` are the anchor cell relative to the top-left visible cell;
/// `row` is negative when the anchor scrolled above the viewport. The image is
/// drawn into `columns` x `rows` cells shifted by the pixel offsets, showing
/// the `source_*` rectangle of the image (in image pixels). Frames list
/// placements sorted by `z` and then by placement order, so a client breaks z
/// ties by list position. A placement may reference an image whose data is
/// still in flight; the client skips it until the image is complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FramePlacement {
    pub image: u32,
    pub column: usize,
    pub row: i32,
    pub columns: usize,
    pub rows: usize,
    pub x_offset: u32,
    pub y_offset: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub z: i32,
}

/// Pane geometry inside the window, in pixels plus the grid size in cells.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayoutPane {
    pub pane: u64,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub cols: usize,
    pub rows: usize,
    pub hidden: bool,
    pub focused: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutTab {
    pub title: String,
    pub panes: Vec<LayoutPane>,
}

/// Complete tab and pane layout; `revision` increases with every change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutFrame {
    pub revision: u64,
    pub active_tab: usize,
    pub tabs: Vec<LayoutTab>,
}

/// Messages sent by the session server.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerMessage {
    Welcome {
        session: String,
    },
    Layout(LayoutFrame),
    Frame(PaneFrame),
    /// Reply to `Copy`; `text` is absent when there was nothing to copy.
    CopyText {
        request: u64,
        text: Option<String>,
    },
    /// Reply to `Layout`; `error` is absent on success.
    LayoutResult {
        request: u64,
        error: Option<String>,
    },
    Error {
        code: String,
        message: String,
    },
    Exit {
        reason: String,
    },
    /// Announces an RGBA image of `pane` (`bytes` = `width * height * 4`). Its
    /// data follows in [`ServerMessage::ImageData`] chunks; a new announcement
    /// for the same pane and image replaces the previous data.
    Image {
        pane: u64,
        image: u32,
        width: u32,
        height: u32,
        bytes: u64,
    },
    /// Non-empty chunk of at most [`MAX_IMAGE_CHUNK_BYTES`] raw bytes (base64 on
    /// the wire). Chunks of one image are contiguous and in order: each
    /// `offset` is the number of bytes sent before it.
    ImageData {
        pane: u64,
        image: u32,
        offset: u64,
        data: Vec<u8>,
    },
    /// The server freed the image data; the client drops its copy.
    ImageRemoved {
        pane: u64,
        image: u32,
    },
}

/// RGBA byte length of a `width` x `height` image, when it fits in `u64`.
pub fn rgba_len(width: u32, height: u32) -> Option<u64> {
    u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(4)
}

/// The [`ServerMessage::Image`] announcement of an RGBA image.
pub fn image_announcement(pane: u64, image: u32, width: u32, height: u32) -> ServerMessage {
    ServerMessage::Image {
        pane,
        image,
        width,
        height,
        bytes: rgba_len(width, height).unwrap_or(u64::MAX),
    }
}

/// Splits `rgba` into contiguous [`ServerMessage::ImageData`] chunks of at most
/// [`MAX_IMAGE_CHUNK_BYTES`] bytes, built one at a time.
pub fn image_data_messages(
    pane: u64,
    image: u32,
    rgba: &[u8],
) -> impl Iterator<Item = ServerMessage> + '_ {
    rgba.chunks(MAX_IMAGE_CHUNK_BYTES)
        .enumerate()
        .map(move |(index, chunk)| ServerMessage::ImageData {
            pane,
            image,
            offset: (index * MAX_IMAGE_CHUNK_BYTES) as u64,
            data: chunk.to_vec(),
        })
}

/// Merges neighbouring cells with equal style into runs.
pub fn line_runs(cells: &[VisibleCell]) -> Vec<StyledRun> {
    let mut runs: Vec<StyledRun> = Vec::new();
    for cell in cells {
        match runs.last_mut() {
            Some(run) if run.matches_cell(cell) => run.text.push(cell.character),
            _ => runs.push(StyledRun {
                text: cell.character.to_string(),
                foreground: cell.foreground,
                background: cell.background,
                bold: cell.bold,
                inverse: cell.inverse,
            }),
        }
    }
    runs
}

/// Exact inverse of [`line_runs`].
pub fn cells_from_runs(runs: &[StyledRun]) -> Vec<VisibleCell> {
    runs.iter()
        .flat_map(|run| {
            run.text.chars().map(|character| VisibleCell {
                character,
                foreground: run.foreground,
                background: run.background,
                bold: run.bold,
                inverse: run.inverse,
            })
        })
        .collect()
}

/// Groups selected `(column, row)` cells into sorted, maximal per-row spans.
pub fn selection_spans(cells: &[(usize, usize)]) -> Vec<SelectionSpan> {
    let mut ordered: Vec<(usize, usize)> =
        cells.iter().map(|&(column, row)| (row, column)).collect();
    ordered.sort_unstable();
    ordered.dedup();
    let mut spans: Vec<SelectionSpan> = Vec::new();
    for (row, column) in ordered {
        match spans.last_mut() {
            Some(span) if span.row == row && span.end.checked_add(1) == Some(column) => {
                span.end = column;
            }
            _ => spans.push(SelectionSpan {
                row,
                start: column,
                end: column,
            }),
        }
    }
    spans
}

/// Expands spans back into `(column, row)` cells in span order.
pub fn selection_cells(spans: &[SelectionSpan]) -> Vec<(usize, usize)> {
    spans
        .iter()
        .flat_map(|span| (span.start..=span.end).map(move |column| (column, span.row)))
        .collect()
}

const SELECTION_KINDS: [SelectionType; 4] = [
    SelectionType::Simple,
    SelectionType::Block,
    SelectionType::Semantic,
    SelectionType::Lines,
];

fn selection_kind_name(kind: SelectionType) -> &'static str {
    match kind {
        SelectionType::Simple => "simple",
        SelectionType::Block => "block",
        SelectionType::Semantic => "semantic",
        SelectionType::Lines => "lines",
    }
}

const BLOCK_OPS: [BlockOp; 3] = [BlockOp::Previous, BlockOp::Next, BlockOp::Select];

fn block_op_name(op: BlockOp) -> &'static str {
    match op {
        BlockOp::Previous => "previous",
        BlockOp::Next => "next",
        BlockOp::Select => "select",
    }
}

const COPY_SOURCES: [CopySource; 3] = [
    CopySource::Selection,
    CopySource::BlockInput,
    CopySource::BlockOutput,
];

fn copy_source_name(source: CopySource) -> &'static str {
    match source {
        CopySource::Selection => "selection",
        CopySource::BlockInput => "block-input",
        CopySource::BlockOutput => "block-output",
    }
}

const SPLIT_DIRECTIONS: [SplitDirection; 2] = [SplitDirection::Right, SplitDirection::Down];

fn split_direction_name(direction: SplitDirection) -> &'static str {
    match direction {
        SplitDirection::Right => "right",
        SplitDirection::Down => "down",
    }
}

const FOCUS_DIRECTIONS: [FocusDirection; 4] = [
    FocusDirection::Left,
    FocusDirection::Right,
    FocusDirection::Up,
    FocusDirection::Down,
];

fn focus_direction_name(direction: FocusDirection) -> &'static str {
    match direction {
        FocusDirection::Left => "left",
        FocusDirection::Right => "right",
        FocusDirection::Up => "up",
        FocusDirection::Down => "down",
    }
}

const CURSOR_SHAPES: [CursorShape; 5] = [
    CursorShape::Block,
    CursorShape::Underline,
    CursorShape::Beam,
    CursorShape::HollowBlock,
    CursorShape::Hidden,
];

fn cursor_shape_name(shape: CursorShape) -> &'static str {
    match shape {
        CursorShape::Block => "block",
        CursorShape::Underline => "underline",
        CursorShape::Beam => "beam",
        CursorShape::HollowBlock => "hollow-block",
        CursorShape::Hidden => "hidden",
    }
}

const DECORATION_COLORS: [BlockDecorationColor; 4] = [
    BlockDecorationColor::Accent,
    BlockDecorationColor::Success,
    BlockDecorationColor::Failure,
    BlockDecorationColor::Muted,
];

fn decoration_color_name(color: BlockDecorationColor) -> &'static str {
    match color {
        BlockDecorationColor::Accent => "accent",
        BlockDecorationColor::Success => "success",
        BlockDecorationColor::Failure => "failure",
        BlockDecorationColor::Muted => "muted",
    }
}

fn encode_color(color: CellColor) -> String {
    match color {
        CellColor::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
        CellColor::Indexed(index) => format!("i{index}"),
        CellColor::Named(name) => format!("n{name}"),
    }
}

fn parse_color(text: &str) -> Option<CellColor> {
    let decimal = |digits: &str| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    if let Some(hex) = text.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
        return Some(CellColor::Rgb(channel(0)?, channel(2)?, channel(4)?));
    }
    if let Some(digits) = text.strip_prefix('i') {
        if !decimal(digits) {
            return None;
        }
        return digits.parse().ok().map(CellColor::Indexed);
    }
    if let Some(digits) = text.strip_prefix('n') {
        if !decimal(digits) {
            return None;
        }
        return digits.parse().ok().map(CellColor::Named);
    }
    None
}

const RUN_BOLD: u64 = 1;
const RUN_INVERSE: u64 = 2;

fn missing(key: &str) -> AttachError {
    AttachError::new(
        AttachErrorCode::MissingField,
        format!("missing field `{key}`"),
    )
}

fn invalid(key: &str) -> AttachError {
    AttachError::new(
        AttachErrorCode::InvalidField,
        format!("invalid field `{key}`"),
    )
}

/// Strict accessor over one JSON object; every error names the field.
struct Fields<'a>(&'a Map<String, Value>);

impl<'a> Fields<'a> {
    fn of(value: &'a Value, what: &str) -> AttachResult<Self> {
        value.as_object().map(Self).ok_or_else(|| invalid(what))
    }

    fn only(&self, allowed: &[&str]) -> AttachResult<()> {
        match self.0.keys().find(|key| !allowed.contains(&key.as_str())) {
            Some(key) => Err(AttachError::new(
                AttachErrorCode::InvalidField,
                format!("unexpected field `{key}`"),
            )),
            None => Ok(()),
        }
    }

    fn value(&self, key: &str) -> AttachResult<&'a Value> {
        self.0.get(key).ok_or_else(|| missing(key))
    }

    fn u64(&self, key: &str) -> AttachResult<u64> {
        self.value(key)?.as_u64().ok_or_else(|| invalid(key))
    }

    fn usize(&self, key: &str) -> AttachResult<usize> {
        usize::try_from(self.u64(key)?).map_err(|_| invalid(key))
    }

    fn u32(&self, key: &str) -> AttachResult<u32> {
        u32::try_from(self.u64(key)?).map_err(|_| invalid(key))
    }

    fn u8(&self, key: &str) -> AttachResult<u8> {
        u8::try_from(self.u64(key)?).map_err(|_| invalid(key))
    }

    fn i32(&self, key: &str) -> AttachResult<i32> {
        let value = self.value(key)?.as_i64().ok_or_else(|| invalid(key))?;
        i32::try_from(value).map_err(|_| invalid(key))
    }

    fn bool(&self, key: &str) -> AttachResult<bool> {
        self.value(key)?.as_bool().ok_or_else(|| invalid(key))
    }

    fn str(&self, key: &str) -> AttachResult<&'a str> {
        self.value(key)?.as_str().ok_or_else(|| invalid(key))
    }

    fn string(&self, key: &str) -> AttachResult<String> {
        self.str(key).map(str::to_owned)
    }

    fn choice<T: Copy>(
        &self,
        key: &str,
        all: &[T],
        name: fn(T) -> &'static str,
    ) -> AttachResult<T> {
        let text = self.str(key)?;
        all.iter()
            .copied()
            .find(|item| name(*item) == text)
            .ok_or_else(|| invalid(key))
    }

    /// Absent means `None`; a present value (including `null`) must be valid.
    fn optional<T>(
        &self,
        key: &str,
        read: fn(&Self, &str) -> AttachResult<T>,
    ) -> AttachResult<Option<T>> {
        if self.0.contains_key(key) {
            read(self, key).map(Some)
        } else {
            Ok(None)
        }
    }

    fn objects<T>(
        &self,
        key: &str,
        read: fn(Fields<'a>) -> AttachResult<T>,
    ) -> AttachResult<Vec<T>> {
        self.value(key)?
            .as_array()
            .ok_or_else(|| invalid(key))?
            .iter()
            .map(|item| read(Fields::of(item, key)?))
            .collect()
    }
}

/// Checks the size, JSON shape, version and type shared by both directions.
fn parse_envelope(line: &[u8]) -> AttachResult<(Map<String, Value>, String)> {
    if line.len() > MAX_ATTACH_LINE_BYTES {
        return Err(AttachError::new(
            AttachErrorCode::LineTooLarge,
            "attach line exceeds 16 MiB",
        ));
    }
    let content = line.strip_suffix(b"\n").unwrap_or(line);
    let content = content.strip_suffix(b"\r").unwrap_or(content);
    let value: Value = serde_json::from_slice(content)
        .map_err(|_| AttachError::new(AttachErrorCode::MalformedJson, "malformed JSON message"))?;
    let Value::Object(object) = value else {
        return Err(AttachError::new(
            AttachErrorCode::InvalidField,
            "message must be a JSON object",
        ));
    };
    let fields = Fields(&object);
    if fields.u64("v")? != ATTACH_PROTOCOL_VERSION {
        return Err(AttachError::new(
            AttachErrorCode::UnknownVersion,
            "unsupported attach protocol version",
        ));
    }
    let kind = fields.string("type")?;
    Ok((object, kind))
}

fn unknown_type(kind: &str) -> AttachError {
    AttachError::new(
        AttachErrorCode::UnknownType,
        format!("unknown message type `{kind}`"),
    )
}

pub fn parse_client(line: &[u8]) -> AttachResult<ClientMessage> {
    let (object, kind) = parse_envelope(line)?;
    let fields = Fields(&object);
    let only = |extra: &[&str]| {
        let mut allowed = vec!["v", "type"];
        allowed.extend_from_slice(extra);
        fields.only(&allowed)
    };
    const METRICS: [&str; 4] = ["width", "height", "cell_width", "cell_height"];
    match kind.as_str() {
        "hello" | "resize" => {
            only(&METRICS)?;
            let (width, height) = (fields.u32("width")?, fields.u32("height")?);
            let (cell_width, cell_height) = (fields.u32("cell_width")?, fields.u32("cell_height")?);
            Ok(if kind == "hello" {
                ClientMessage::Hello {
                    width,
                    height,
                    cell_width,
                    cell_height,
                }
            } else {
                ClientMessage::Resize {
                    width,
                    height,
                    cell_width,
                    cell_height,
                }
            })
        }
        "input" => {
            only(&["pane", "data"])?;
            let pane = fields.u64("pane")?;
            let data = STANDARD
                .decode(fields.str("data")?)
                .map_err(|_| invalid("data"))?;
            Ok(ClientMessage::Input { pane, data })
        }
        "scroll" => {
            only(&["pane", "lines"])?;
            Ok(ClientMessage::Scroll {
                pane: fields.u64("pane")?,
                lines: fields.i32("lines")?,
            })
        }
        "scroll-to-bottom" => {
            only(&["pane"])?;
            Ok(ClientMessage::ScrollToBottom {
                pane: fields.u64("pane")?,
            })
        }
        "selection" => {
            let op = match fields.str("op")? {
                "begin" => {
                    only(&["pane", "op", "column", "row", "kind"])?;
                    SelectionOp::Begin {
                        column: fields.usize("column")?,
                        row: fields.usize("row")?,
                        kind: fields.choice("kind", &SELECTION_KINDS, selection_kind_name)?,
                    }
                }
                "extend" => {
                    only(&["pane", "op", "column", "row"])?;
                    SelectionOp::Extend {
                        column: fields.usize("column")?,
                        row: fields.usize("row")?,
                    }
                }
                "clear" => {
                    only(&["pane", "op"])?;
                    SelectionOp::Clear
                }
                _ => return Err(invalid("op")),
            };
            Ok(ClientMessage::Selection {
                pane: fields.u64("pane")?,
                op,
            })
        }
        "block" => {
            only(&["pane", "op"])?;
            Ok(ClientMessage::Block {
                pane: fields.u64("pane")?,
                op: fields.choice("op", &BLOCK_OPS, block_op_name)?,
            })
        }
        "copy" => {
            only(&["request", "pane", "source"])?;
            Ok(ClientMessage::Copy {
                request: fields.u64("request")?,
                pane: fields.u64("pane")?,
                source: fields.choice("source", &COPY_SOURCES, copy_source_name)?,
            })
        }
        "layout" => {
            let request = fields.u64("request")?;
            let base = ["request", "action"];
            let action = match fields.str("action")? {
                simple @ ("new-tab" | "close-tab" | "next-tab" | "previous-tab" | "close-pane") => {
                    only(&base)?;
                    match simple {
                        "new-tab" => LayoutAction::NewTab,
                        "close-tab" => LayoutAction::CloseTab,
                        "next-tab" => LayoutAction::NextTab,
                        "previous-tab" => LayoutAction::PreviousTab,
                        _ => LayoutAction::ClosePane,
                    }
                }
                "select-tab" => {
                    only(&["request", "action", "index"])?;
                    LayoutAction::SelectTab {
                        index: fields.usize("index")?,
                    }
                }
                "split" => {
                    only(&["request", "action", "direction"])?;
                    LayoutAction::Split {
                        direction: fields.choice(
                            "direction",
                            &SPLIT_DIRECTIONS,
                            split_direction_name,
                        )?,
                    }
                }
                "focus-direction" => {
                    only(&["request", "action", "direction"])?;
                    LayoutAction::FocusDirection {
                        direction: fields.choice(
                            "direction",
                            &FOCUS_DIRECTIONS,
                            focus_direction_name,
                        )?,
                    }
                }
                "focus-pane" => {
                    only(&["request", "action", "pane"])?;
                    LayoutAction::FocusPane {
                        pane: fields.u64("pane")?,
                    }
                }
                _ => return Err(invalid("action")),
            };
            Ok(ClientMessage::Layout { request, action })
        }
        "resync" => {
            only(&["pane"])?;
            Ok(ClientMessage::Resync {
                pane: fields.optional("pane", Fields::u64)?,
            })
        }
        "detach" => {
            only(&[])?;
            Ok(ClientMessage::Detach)
        }
        _ => Err(unknown_type(&kind)),
    }
}

fn layout_action_fields(action: LayoutAction) -> Vec<(&'static str, Value)> {
    let (name, extra) = match action {
        LayoutAction::NewTab => ("new-tab", None),
        LayoutAction::CloseTab => ("close-tab", None),
        LayoutAction::NextTab => ("next-tab", None),
        LayoutAction::PreviousTab => ("previous-tab", None),
        LayoutAction::ClosePane => ("close-pane", None),
        LayoutAction::SelectTab { index } => ("select-tab", Some(("index", json!(index)))),
        LayoutAction::Split { direction } => (
            "split",
            Some(("direction", json!(split_direction_name(direction)))),
        ),
        LayoutAction::FocusDirection { direction } => (
            "focus-direction",
            Some(("direction", json!(focus_direction_name(direction)))),
        ),
        LayoutAction::FocusPane { pane } => ("focus-pane", Some(("pane", json!(pane)))),
    };
    let mut fields = vec![("action", json!(name))];
    fields.extend(extra);
    fields
}

/// Builds one envelope and serializes it as a single newline-terminated line.
fn encode_line(kind: &str, fields: Vec<(&str, Value)>) -> AttachResult<Vec<u8>> {
    let mut object = Map::new();
    object.insert("v".to_owned(), json!(ATTACH_PROTOCOL_VERSION));
    object.insert("type".to_owned(), json!(kind));
    for (key, value) in fields {
        object.insert(key.to_owned(), value);
    }
    // Serializing a `Value` cannot fail, and JSON strings escape raw newlines.
    let mut bytes = Value::Object(object).to_string().into_bytes();
    bytes.push(b'\n');
    if bytes.len() > MAX_ATTACH_LINE_BYTES {
        return Err(AttachError::new(
            AttachErrorCode::LineTooLarge,
            "encoded attach line exceeds 16 MiB",
        ));
    }
    Ok(bytes)
}

pub fn encode_client(message: &ClientMessage) -> AttachResult<Vec<u8>> {
    let metrics = |width: u32, height: u32, cell_width: u32, cell_height: u32| {
        vec![
            ("width", json!(width)),
            ("height", json!(height)),
            ("cell_width", json!(cell_width)),
            ("cell_height", json!(cell_height)),
        ]
    };
    let (kind, fields) = match message {
        ClientMessage::Hello {
            width,
            height,
            cell_width,
            cell_height,
        } => ("hello", metrics(*width, *height, *cell_width, *cell_height)),
        ClientMessage::Resize {
            width,
            height,
            cell_width,
            cell_height,
        } => (
            "resize",
            metrics(*width, *height, *cell_width, *cell_height),
        ),
        ClientMessage::Input { pane, data } => (
            "input",
            vec![
                ("pane", json!(pane)),
                ("data", json!(STANDARD.encode(data))),
            ],
        ),
        ClientMessage::Scroll { pane, lines } => (
            "scroll",
            vec![("pane", json!(pane)), ("lines", json!(lines))],
        ),
        ClientMessage::ScrollToBottom { pane } => ("scroll-to-bottom", vec![("pane", json!(pane))]),
        ClientMessage::Selection { pane, op } => {
            let mut fields = vec![("pane", json!(pane))];
            match op {
                SelectionOp::Begin { column, row, kind } => fields.extend([
                    ("op", json!("begin")),
                    ("column", json!(column)),
                    ("row", json!(row)),
                    ("kind", json!(selection_kind_name(*kind))),
                ]),
                SelectionOp::Extend { column, row } => fields.extend([
                    ("op", json!("extend")),
                    ("column", json!(column)),
                    ("row", json!(row)),
                ]),
                SelectionOp::Clear => fields.push(("op", json!("clear"))),
            }
            ("selection", fields)
        }
        ClientMessage::Block { pane, op } => (
            "block",
            vec![("pane", json!(pane)), ("op", json!(block_op_name(*op)))],
        ),
        ClientMessage::Copy {
            request,
            pane,
            source,
        } => (
            "copy",
            vec![
                ("request", json!(request)),
                ("pane", json!(pane)),
                ("source", json!(copy_source_name(*source))),
            ],
        ),
        ClientMessage::Layout { request, action } => {
            let mut fields = vec![("request", json!(request))];
            fields.extend(layout_action_fields(*action));
            ("layout", fields)
        }
        ClientMessage::Resync { pane } => (
            "resync",
            pane.map(|pane| vec![("pane", json!(pane))])
                .unwrap_or_default(),
        ),
        ClientMessage::Detach => ("detach", Vec::new()),
    };
    encode_line(kind, fields)
}

fn run_value(run: &StyledRun) -> Value {
    let flags = if run.bold { RUN_BOLD } else { 0 } | if run.inverse { RUN_INVERSE } else { 0 };
    json!([
        run.text,
        encode_color(run.foreground),
        encode_color(run.background),
        flags
    ])
}

fn parse_run(value: &Value) -> AttachResult<StyledRun> {
    let item = match value.as_array().map(Vec::as_slice) {
        Some([text, foreground, background, flags]) => (text, foreground, background, flags),
        _ => return Err(invalid("runs")),
    };
    let text = item
        .0
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| invalid("runs"))?;
    let color = |value: &Value| {
        value
            .as_str()
            .and_then(parse_color)
            .ok_or_else(|| invalid("runs"))
    };
    let flags = item
        .3
        .as_u64()
        .filter(|flags| flags & !(RUN_BOLD | RUN_INVERSE) == 0)
        .ok_or_else(|| invalid("runs"))?;
    Ok(StyledRun {
        text: text.to_owned(),
        foreground: color(item.1)?,
        background: color(item.2)?,
        bold: flags & RUN_BOLD != 0,
        inverse: flags & RUN_INVERSE != 0,
    })
}

fn frame_fields(frame: &PaneFrame) -> Vec<(&'static str, Value)> {
    let lines: Vec<Value> = frame
        .lines
        .iter()
        .map(|line| json!({ "row": line.row, "runs": line.runs.iter().map(run_value).collect::<Vec<_>>() }))
        .collect();
    let selection: Vec<Value> = frame
        .selection
        .iter()
        .map(|span| json!({ "row": span.row, "start": span.start, "end": span.end }))
        .collect();
    let decorations: Vec<Value> = frame
        .decorations
        .iter()
        .map(|decoration| {
            let mut object = Map::new();
            object.insert("block_id".to_owned(), json!(decoration.block_id));
            object.insert("marker_row".to_owned(), json!(decoration.marker_row));
            if let Some(row) = decoration.separator_row {
                object.insert("separator_row".to_owned(), json!(row));
            }
            object.insert(
                "color".to_owned(),
                json!(decoration_color_name(decoration.color)),
            );
            Value::Object(object)
        })
        .collect();
    let modes = &frame.modes;
    vec![
        ("pane", json!(frame.pane)),
        ("seq", json!(frame.seq)),
        ("full", json!(frame.full)),
        ("cols", json!(frame.cols)),
        ("rows", json!(frame.rows)),
        ("lines", Value::Array(lines)),
        (
            "cursor",
            json!({
                "column": frame.cursor.column,
                "row": frame.cursor.row,
                "shape": cursor_shape_name(frame.cursor.shape),
            }),
        ),
        ("selection", Value::Array(selection)),
        (
            "modes",
            json!({
                "keyboard_flags": modes.keyboard_flags,
                "app_cursor": modes.app_cursor,
                "bracketed_paste": modes.bracketed_paste,
                "mouse_or_alternate": modes.mouse_or_alternate,
                "alternate_screen": modes.alternate_screen,
            }),
        ),
        ("decorations", Value::Array(decorations)),
        ("title", json!(frame.title)),
        ("cwd", json!(frame.cwd)),
        (
            "placements",
            Value::Array(frame.placements.iter().map(placement_value).collect()),
        ),
    ]
}

fn placement_value(placement: &FramePlacement) -> Value {
    json!({
        "image": placement.image,
        "column": placement.column,
        "row": placement.row,
        "columns": placement.columns,
        "rows": placement.rows,
        "x_offset": placement.x_offset,
        "y_offset": placement.y_offset,
        "source_x": placement.source_x,
        "source_y": placement.source_y,
        "source_width": placement.source_width,
        "source_height": placement.source_height,
        "z": placement.z,
    })
}

fn parse_placement(fields: Fields<'_>) -> AttachResult<FramePlacement> {
    fields.only(&[
        "image",
        "column",
        "row",
        "columns",
        "rows",
        "x_offset",
        "y_offset",
        "source_x",
        "source_y",
        "source_width",
        "source_height",
        "z",
    ])?;
    let positive = |key: &str| -> AttachResult<u64> {
        let value = fields.u64(key)?;
        if value == 0 {
            return Err(invalid(key));
        }
        Ok(value)
    };
    Ok(FramePlacement {
        image: fields.u32("image")?,
        column: fields.usize("column")?,
        row: fields.i32("row")?,
        columns: usize::try_from(positive("columns")?).map_err(|_| invalid("columns"))?,
        rows: usize::try_from(positive("rows")?).map_err(|_| invalid("rows"))?,
        x_offset: fields.u32("x_offset")?,
        y_offset: fields.u32("y_offset")?,
        source_x: fields.u32("source_x")?,
        source_y: fields.u32("source_y")?,
        source_width: u32::try_from(positive("source_width")?)
            .map_err(|_| invalid("source_width"))?,
        source_height: u32::try_from(positive("source_height")?)
            .map_err(|_| invalid("source_height"))?,
        z: fields.i32("z")?,
    })
}

fn parse_frame(fields: &Fields<'_>) -> AttachResult<PaneFrame> {
    fields.only(&[
        "v",
        "type",
        "pane",
        "seq",
        "full",
        "cols",
        "rows",
        "lines",
        "cursor",
        "selection",
        "modes",
        "decorations",
        "title",
        "cwd",
        "placements",
    ])?;
    let lines = fields.objects("lines", |line| {
        line.only(&["row", "runs"])?;
        let runs = line
            .value("runs")?
            .as_array()
            .ok_or_else(|| invalid("runs"))?;
        Ok(FrameLine {
            row: line.usize("row")?,
            runs: runs.iter().map(parse_run).collect::<AttachResult<_>>()?,
        })
    })?;
    let cursor = Fields::of(fields.value("cursor")?, "cursor")?;
    cursor.only(&["column", "row", "shape"])?;
    let cursor = Cursor {
        column: cursor.usize("column")?,
        row: cursor.usize("row")?,
        shape: cursor.choice("shape", &CURSOR_SHAPES, cursor_shape_name)?,
    };
    let selection = fields.objects("selection", |span| {
        span.only(&["row", "start", "end"])?;
        let (start, end) = (span.usize("start")?, span.usize("end")?);
        if start > end {
            return Err(invalid("selection"));
        }
        Ok(SelectionSpan {
            row: span.usize("row")?,
            start,
            end,
        })
    })?;
    let modes = Fields::of(fields.value("modes")?, "modes")?;
    modes.only(&[
        "keyboard_flags",
        "app_cursor",
        "bracketed_paste",
        "mouse_or_alternate",
        "alternate_screen",
    ])?;
    let modes = PaneModes {
        keyboard_flags: modes.u8("keyboard_flags")?,
        app_cursor: modes.bool("app_cursor")?,
        bracketed_paste: modes.bool("bracketed_paste")?,
        mouse_or_alternate: modes.bool("mouse_or_alternate")?,
        alternate_screen: modes.bool("alternate_screen")?,
    };
    let decorations = fields.objects("decorations", |decoration| {
        decoration.only(&["block_id", "marker_row", "separator_row", "color"])?;
        Ok(BlockDecoration {
            block_id: decoration.u64("block_id")?,
            marker_row: decoration.usize("marker_row")?,
            separator_row: decoration.optional("separator_row", Fields::usize)?,
            color: decoration.choice("color", &DECORATION_COLORS, decoration_color_name)?,
        })
    })?;
    Ok(PaneFrame {
        pane: fields.u64("pane")?,
        seq: fields.u64("seq")?,
        full: fields.bool("full")?,
        cols: fields.usize("cols")?,
        rows: fields.usize("rows")?,
        lines,
        cursor,
        selection,
        modes,
        decorations,
        title: fields.string("title")?,
        cwd: fields.string("cwd")?,
        placements: fields.objects("placements", parse_placement)?,
    })
}

fn layout_fields(layout: &LayoutFrame) -> Vec<(&'static str, Value)> {
    let tabs: Vec<Value> = layout
        .tabs
        .iter()
        .map(|tab| {
            let panes: Vec<Value> = tab
                .panes
                .iter()
                .map(|pane| {
                    json!({
                        "pane": pane.pane,
                        "x": pane.x,
                        "y": pane.y,
                        "width": pane.width,
                        "height": pane.height,
                        "cols": pane.cols,
                        "rows": pane.rows,
                        "hidden": pane.hidden,
                        "focused": pane.focused,
                    })
                })
                .collect();
            json!({ "title": tab.title, "panes": panes })
        })
        .collect();
    vec![
        ("revision", json!(layout.revision)),
        ("active_tab", json!(layout.active_tab)),
        ("tabs", Value::Array(tabs)),
    ]
}

fn parse_layout(fields: &Fields<'_>) -> AttachResult<LayoutFrame> {
    fields.only(&["v", "type", "revision", "active_tab", "tabs"])?;
    let tabs = fields.objects("tabs", |tab| {
        tab.only(&["title", "panes"])?;
        let panes = tab.objects("panes", |pane| {
            pane.only(&[
                "pane", "x", "y", "width", "height", "cols", "rows", "hidden", "focused",
            ])?;
            Ok(LayoutPane {
                pane: pane.u64("pane")?,
                x: pane.u32("x")?,
                y: pane.u32("y")?,
                width: pane.u32("width")?,
                height: pane.u32("height")?,
                cols: pane.usize("cols")?,
                rows: pane.usize("rows")?,
                hidden: pane.bool("hidden")?,
                focused: pane.bool("focused")?,
            })
        })?;
        Ok(LayoutTab {
            title: tab.string("title")?,
            panes,
        })
    })?;
    Ok(LayoutFrame {
        revision: fields.u64("revision")?,
        active_tab: fields.usize("active_tab")?,
        tabs,
    })
}

pub fn encode_server(message: &ServerMessage) -> AttachResult<Vec<u8>> {
    let (kind, fields) = match message {
        ServerMessage::Welcome { session } => ("welcome", vec![("session", json!(session))]),
        ServerMessage::Layout(layout) => ("layout", layout_fields(layout)),
        ServerMessage::Frame(frame) => ("frame", frame_fields(frame)),
        ServerMessage::CopyText { request, text } => {
            let mut fields = vec![("request", json!(request))];
            fields.extend(text.as_ref().map(|text| ("text", json!(text))));
            ("copy-text", fields)
        }
        ServerMessage::LayoutResult { request, error } => {
            let mut fields = vec![("request", json!(request))];
            fields.extend(error.as_ref().map(|error| ("error", json!(error))));
            ("layout-result", fields)
        }
        ServerMessage::Error { code, message } => (
            "error",
            vec![("code", json!(code)), ("message", json!(message))],
        ),
        ServerMessage::Exit { reason } => ("exit", vec![("reason", json!(reason))]),
        ServerMessage::Image {
            pane,
            image,
            width,
            height,
            bytes,
        } => (
            "image",
            vec![
                ("pane", json!(pane)),
                ("image", json!(image)),
                ("width", json!(width)),
                ("height", json!(height)),
                ("bytes", json!(bytes)),
            ],
        ),
        ServerMessage::ImageData {
            pane,
            image,
            offset,
            data,
        } => {
            if data.len() > MAX_IMAGE_CHUNK_BYTES {
                return Err(AttachError::new(
                    AttachErrorCode::InvalidField,
                    "image chunk exceeds 4 MiB",
                ));
            }
            (
                "image-data",
                vec![
                    ("pane", json!(pane)),
                    ("image", json!(image)),
                    ("offset", json!(offset)),
                    ("data", json!(STANDARD.encode(data))),
                ],
            )
        }
        ServerMessage::ImageRemoved { pane, image } => (
            "image-removed",
            vec![("pane", json!(pane)), ("image", json!(image))],
        ),
    };
    encode_line(kind, fields)
}

pub fn parse_server(line: &[u8]) -> AttachResult<ServerMessage> {
    let (object, kind) = parse_envelope(line)?;
    let fields = Fields(&object);
    let only = |extra: &[&str]| {
        let mut allowed = vec!["v", "type"];
        allowed.extend_from_slice(extra);
        fields.only(&allowed)
    };
    match kind.as_str() {
        "welcome" => {
            only(&["session"])?;
            Ok(ServerMessage::Welcome {
                session: fields.string("session")?,
            })
        }
        "layout" => parse_layout(&fields).map(ServerMessage::Layout),
        "frame" => parse_frame(&fields).map(ServerMessage::Frame),
        "copy-text" => {
            only(&["request", "text"])?;
            Ok(ServerMessage::CopyText {
                request: fields.u64("request")?,
                text: fields.optional("text", Fields::string)?,
            })
        }
        "layout-result" => {
            only(&["request", "error"])?;
            Ok(ServerMessage::LayoutResult {
                request: fields.u64("request")?,
                error: fields.optional("error", Fields::string)?,
            })
        }
        "error" => {
            only(&["code", "message"])?;
            Ok(ServerMessage::Error {
                code: fields.string("code")?,
                message: fields.string("message")?,
            })
        }
        "exit" => {
            only(&["reason"])?;
            Ok(ServerMessage::Exit {
                reason: fields.string("reason")?,
            })
        }
        "image" => {
            only(&["pane", "image", "width", "height", "bytes"])?;
            let (width, height) = (fields.u32("width")?, fields.u32("height")?);
            if width == 0 {
                return Err(invalid("width"));
            }
            if height == 0 {
                return Err(invalid("height"));
            }
            let bytes = fields.u64("bytes")?;
            if rgba_len(width, height) != Some(bytes) {
                return Err(invalid("bytes"));
            }
            Ok(ServerMessage::Image {
                pane: fields.u64("pane")?,
                image: fields.u32("image")?,
                width,
                height,
                bytes,
            })
        }
        "image-data" => {
            only(&["pane", "image", "offset", "data"])?;
            let data = STANDARD
                .decode(fields.str("data")?)
                .map_err(|_| invalid("data"))?;
            if data.is_empty() || data.len() > MAX_IMAGE_CHUNK_BYTES {
                return Err(invalid("data"));
            }
            Ok(ServerMessage::ImageData {
                pane: fields.u64("pane")?,
                image: fields.u32("image")?,
                offset: fields.u64("offset")?,
                data,
            })
        }
        "image-removed" => {
            only(&["pane", "image"])?;
            Ok(ServerMessage::ImageRemoved {
                pane: fields.u64("pane")?,
                image: fields.u32("image")?,
            })
        }
        _ => Err(unknown_type(&kind)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CellColor, Cursor, VisibleCell};
    use crate::renderer::{BlockDecoration, BlockDecorationColor};
    use alacritty_terminal::selection::SelectionType;
    use serde_json::{Value, json};
    use vte::ansi::CursorShape;

    fn assert_single_line(bytes: &[u8]) {
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    }

    fn every_client_message() -> Vec<ClientMessage> {
        let mut messages = vec![
            ClientMessage::Hello {
                width: 1280,
                height: 720,
                cell_width: 8,
                cell_height: 16,
            },
            ClientMessage::Input {
                pane: 3,
                data: vec![0, 27, b'[', b'A', 0xff],
            },
            ClientMessage::Input {
                pane: 3,
                data: Vec::new(),
            },
            ClientMessage::Resize {
                width: 640,
                height: 480,
                cell_width: 9,
                cell_height: 18,
            },
            ClientMessage::Scroll { pane: 1, lines: 5 },
            ClientMessage::Scroll {
                pane: 1,
                lines: i32::MIN,
            },
            ClientMessage::ScrollToBottom { pane: 2 },
            ClientMessage::Selection {
                pane: 4,
                op: SelectionOp::Extend { column: 7, row: 2 },
            },
            ClientMessage::Selection {
                pane: 4,
                op: SelectionOp::Clear,
            },
            ClientMessage::Resync { pane: None },
            ClientMessage::Resync { pane: Some(8) },
            ClientMessage::Detach,
        ];
        for kind in [
            SelectionType::Simple,
            SelectionType::Block,
            SelectionType::Semantic,
            SelectionType::Lines,
        ] {
            messages.push(ClientMessage::Selection {
                pane: 4,
                op: SelectionOp::Begin {
                    column: 1,
                    row: 0,
                    kind,
                },
            });
        }
        for op in [BlockOp::Previous, BlockOp::Next, BlockOp::Select] {
            messages.push(ClientMessage::Block { pane: 5, op });
        }
        for source in [
            CopySource::Selection,
            CopySource::BlockInput,
            CopySource::BlockOutput,
        ] {
            messages.push(ClientMessage::Copy {
                request: 11,
                pane: 5,
                source,
            });
        }
        for action in [
            LayoutAction::NewTab,
            LayoutAction::CloseTab,
            LayoutAction::NextTab,
            LayoutAction::PreviousTab,
            LayoutAction::SelectTab { index: 2 },
            LayoutAction::Split {
                direction: SplitDirection::Right,
            },
            LayoutAction::Split {
                direction: SplitDirection::Down,
            },
            LayoutAction::ClosePane,
            LayoutAction::FocusDirection {
                direction: FocusDirection::Left,
            },
            LayoutAction::FocusDirection {
                direction: FocusDirection::Right,
            },
            LayoutAction::FocusDirection {
                direction: FocusDirection::Up,
            },
            LayoutAction::FocusDirection {
                direction: FocusDirection::Down,
            },
            LayoutAction::FocusPane { pane: 9 },
        ] {
            messages.push(ClientMessage::Layout {
                request: 12,
                action,
            });
        }
        messages
    }

    fn run(text: &str, foreground: CellColor, background: CellColor, bold: bool) -> StyledRun {
        StyledRun {
            text: text.into(),
            foreground,
            background,
            bold,
            inverse: !bold,
        }
    }

    fn sample_frame(shape: CursorShape) -> PaneFrame {
        PaneFrame {
            pane: 7,
            seq: 42,
            full: true,
            cols: 80,
            rows: 24,
            lines: vec![
                FrameLine {
                    row: 0,
                    runs: vec![
                        run(
                            "ab",
                            CellColor::Rgb(255, 0, 16),
                            CellColor::Indexed(0),
                            true,
                        ),
                        run(
                            "\u{e9}x",
                            CellColor::Named(256),
                            CellColor::Named(u16::MAX),
                            false,
                        ),
                    ],
                },
                FrameLine {
                    row: 23,
                    runs: Vec::new(),
                },
            ],
            cursor: Cursor {
                column: 3,
                row: 1,
                shape,
            },
            selection: vec![SelectionSpan {
                row: 0,
                start: 1,
                end: 1,
            }],
            modes: PaneModes {
                keyboard_flags: 31,
                app_cursor: true,
                bracketed_paste: false,
                mouse_or_alternate: true,
                alternate_screen: false,
            },
            decorations: vec![
                BlockDecoration {
                    block_id: 1,
                    marker_row: 0,
                    separator_row: None,
                    color: BlockDecorationColor::Accent,
                },
                BlockDecoration {
                    block_id: 2,
                    marker_row: 3,
                    separator_row: Some(9),
                    color: BlockDecorationColor::Success,
                },
                BlockDecoration {
                    block_id: 3,
                    marker_row: 10,
                    separator_row: Some(11),
                    color: BlockDecorationColor::Failure,
                },
                BlockDecoration {
                    block_id: 4,
                    marker_row: 12,
                    separator_row: None,
                    color: BlockDecorationColor::Muted,
                },
            ],
            title: "vim".into(),
            cwd: "/home/user".into(),
            placements: vec![
                FramePlacement {
                    image: 9,
                    column: 2,
                    row: -3,
                    columns: 4,
                    rows: 5,
                    x_offset: 1,
                    y_offset: 2,
                    source_x: 0,
                    source_y: 4,
                    source_width: 30,
                    source_height: 40,
                    z: -1_073_741_825,
                },
                FramePlacement {
                    image: u32::MAX,
                    column: 0,
                    row: 7,
                    columns: 1,
                    rows: 1,
                    x_offset: 0,
                    y_offset: 0,
                    source_x: 0,
                    source_y: 0,
                    source_width: 1,
                    source_height: 1,
                    z: 3,
                },
            ],
        }
    }

    fn every_server_message() -> Vec<ServerMessage> {
        let mut messages = vec![
            ServerMessage::Welcome {
                session: "default".into(),
            },
            ServerMessage::Layout(LayoutFrame {
                revision: 3,
                active_tab: 1,
                tabs: vec![
                    LayoutTab {
                        title: "so1sh".into(),
                        panes: Vec::new(),
                    },
                    LayoutTab {
                        title: "src".into(),
                        panes: vec![
                            LayoutPane {
                                pane: 1,
                                x: 0,
                                y: 16,
                                width: 640,
                                height: 704,
                                cols: 80,
                                rows: 44,
                                hidden: false,
                                focused: true,
                            },
                            LayoutPane {
                                pane: 2,
                                x: 641,
                                y: 16,
                                width: 639,
                                height: 704,
                                cols: 79,
                                rows: 44,
                                hidden: true,
                                focused: false,
                            },
                        ],
                    },
                ],
            }),
            ServerMessage::Frame(PaneFrame {
                full: false,
                title: String::new(),
                cwd: String::new(),
                ..sample_frame(CursorShape::Block)
            }),
            ServerMessage::CopyText {
                request: 5,
                text: Some("echo hi\n".into()),
            },
            ServerMessage::CopyText {
                request: 6,
                text: None,
            },
            ServerMessage::LayoutResult {
                request: 7,
                error: None,
            },
            ServerMessage::LayoutResult {
                request: 8,
                error: Some("pane too small".into()),
            },
            ServerMessage::Error {
                code: "ALREADY_ATTACHED".into(),
                message: "another client is attached".into(),
            },
            ServerMessage::Exit {
                reason: "last pane exited".into(),
            },
            ServerMessage::Image {
                pane: 2,
                image: 9,
                width: 3,
                height: 2,
                bytes: 24,
            },
            ServerMessage::ImageData {
                pane: 2,
                image: 9,
                offset: 16,
                data: vec![0, 1, 2, 255, 7, 8, 9, 10],
            },
            ServerMessage::ImageRemoved { pane: 2, image: 9 },
            ServerMessage::Frame(PaneFrame {
                placements: Vec::new(),
                ..sample_frame(CursorShape::Beam)
            }),
        ];
        for shape in [
            CursorShape::Block,
            CursorShape::Underline,
            CursorShape::Beam,
            CursorShape::HollowBlock,
            CursorShape::Hidden,
        ] {
            messages.push(ServerMessage::Frame(sample_frame(shape)));
        }
        messages
    }

    #[test]
    fn client_messages_round_trip_through_one_json_line() {
        for message in every_client_message() {
            let bytes = encode_client(&message).unwrap();
            assert_single_line(&bytes);
            assert_eq!(parse_client(&bytes).unwrap(), message, "{message:?}");
        }
    }

    #[test]
    fn server_messages_round_trip_through_one_json_line() {
        for message in every_server_message() {
            let bytes = encode_server(&message).unwrap();
            assert_single_line(&bytes);
            assert_eq!(parse_server(&bytes).unwrap(), message, "{message:?}");
        }
    }

    #[test]
    fn wire_format_is_pinned_for_representative_messages() {
        let wire = |bytes: Vec<u8>| -> Value { serde_json::from_slice(&bytes).unwrap() };
        assert_eq!(
            wire(
                encode_client(&ClientMessage::Input {
                    pane: 2,
                    data: b"hi".to_vec()
                })
                .unwrap()
            ),
            json!({"v":1,"type":"input","pane":2,"data":"aGk="})
        );
        assert_eq!(
            wire(
                encode_client(&ClientMessage::Selection {
                    pane: 1,
                    op: SelectionOp::Begin {
                        column: 2,
                        row: 3,
                        kind: SelectionType::Semantic
                    }
                })
                .unwrap()
            ),
            json!({"v":1,"type":"selection","pane":1,"op":"begin","column":2,"row":3,"kind":"semantic"})
        );
        assert_eq!(
            wire(
                encode_client(&ClientMessage::Layout {
                    request: 4,
                    action: LayoutAction::FocusDirection {
                        direction: FocusDirection::Up
                    }
                })
                .unwrap()
            ),
            json!({"v":1,"type":"layout","request":4,"action":"focus-direction","direction":"up"})
        );
        assert_eq!(
            wire(encode_client(&ClientMessage::Resync { pane: None }).unwrap()),
            json!({"v":1,"type":"resync"})
        );
        let frame = wire(
            encode_server(&ServerMessage::Frame(sample_frame(
                CursorShape::HollowBlock,
            )))
            .unwrap(),
        );
        assert_eq!(frame["type"], "frame");
        assert_eq!(
            frame["lines"][0]["runs"],
            json!([["ab", "#ff0010", "i0", 1], ["\u{e9}x", "n256", "n65535", 2]])
        );
        assert_eq!(
            frame["cursor"],
            json!({"column":3,"row":1,"shape":"hollow-block"})
        );
        assert_eq!(frame["selection"], json!([{"row":0,"start":1,"end":1}]));
        assert_eq!(
            frame["decorations"][0],
            json!({"block_id":1,"marker_row":0,"color":"accent"})
        );
        assert_eq!(frame["decorations"][1]["separator_row"], 9);
        assert_eq!(
            wire(
                encode_server(&ServerMessage::CopyText {
                    request: 1,
                    text: None
                })
                .unwrap()
            ),
            json!({"v":1,"type":"copy-text","request":1})
        );
        assert_eq!(
            frame["placements"][0],
            json!({
                "image": 9, "column": 2, "row": -3, "columns": 4, "rows": 5,
                "x_offset": 1, "y_offset": 2, "source_x": 0, "source_y": 4,
                "source_width": 30, "source_height": 40, "z": -1_073_741_825
            })
        );
        assert_eq!(
            wire(
                encode_server(&ServerMessage::Image {
                    pane: 1,
                    image: 5,
                    width: 2,
                    height: 1,
                    bytes: 8
                })
                .unwrap()
            ),
            json!({"v":1,"type":"image","pane":1,"image":5,"width":2,"height":1,"bytes":8})
        );
        assert_eq!(
            wire(
                encode_server(&ServerMessage::ImageData {
                    pane: 1,
                    image: 5,
                    offset: 4,
                    data: b"hi".to_vec()
                })
                .unwrap()
            ),
            json!({"v":1,"type":"image-data","pane":1,"image":5,"offset":4,"data":"aGk="})
        );
        assert_eq!(
            wire(encode_server(&ServerMessage::ImageRemoved { pane: 1, image: 5 }).unwrap()),
            json!({"v":1,"type":"image-removed","pane":1,"image":5})
        );
    }

    #[test]
    fn parser_accepts_crlf_terminated_lines() {
        assert_eq!(
            parse_client(b"{\"v\":1,\"type\":\"detach\"}\r\n").unwrap(),
            ClientMessage::Detach
        );
    }

    #[test]
    fn client_parser_reports_every_error_class() {
        use AttachErrorCode::*;
        let cases = [
            ("{", MalformedJson),
            ("[1]", InvalidField),
            (r#"{"type":"detach"}"#, MissingField),
            (r#"{"v":"1","type":"detach"}"#, InvalidField),
            (r#"{"v":2,"type":"detach"}"#, UnknownVersion),
            (r#"{"v":1}"#, MissingField),
            (r#"{"v":1,"type":7}"#, InvalidField),
            (r#"{"v":1,"type":"teleport"}"#, UnknownType),
            (r#"{"v":1,"type":"detach","pane":1}"#, InvalidField),
            (
                r#"{"v":1,"type":"hello","width":1,"height":1,"cell_width":1}"#,
                MissingField,
            ),
            (
                r#"{"v":1,"type":"hello","width":1,"height":1,"cell_width":1,"cell_height":4294967296}"#,
                InvalidField,
            ),
            (r#"{"v":1,"type":"input","pane":1}"#, MissingField),
            (
                r#"{"v":1,"type":"input","pane":"1","data":""}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"input","pane":-1,"data":""}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"input","pane":1,"data":"***"}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"scroll","pane":1,"lines":2147483648}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"scroll","pane":1,"lines":1.5}"#,
                InvalidField,
            ),
            (r#"{"v":1,"type":"scroll-to-bottom"}"#, MissingField),
            (r#"{"v":1,"type":"selection","pane":1}"#, MissingField),
            (
                r#"{"v":1,"type":"selection","pane":1,"op":"grow"}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"selection","pane":1,"op":"begin","column":0,"row":0}"#,
                MissingField,
            ),
            (
                r#"{"v":1,"type":"selection","pane":1,"op":"begin","column":0,"row":0,"kind":"word"}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"selection","pane":1,"op":"clear","row":0}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"selection","pane":1,"op":"extend","column":0,"row":0,"kind":"simple"}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"block","pane":1,"op":"first"}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"copy","request":1,"pane":1}"#,
                MissingField,
            ),
            (
                r#"{"v":1,"type":"copy","request":1,"pane":1,"source":"screen"}"#,
                InvalidField,
            ),
            (r#"{"v":1,"type":"layout","request":1}"#, MissingField),
            (
                r#"{"v":1,"type":"layout","request":1,"action":"zoom"}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"layout","request":1,"action":"new-tab","index":1}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"layout","request":1,"action":"select-tab"}"#,
                MissingField,
            ),
            (
                r#"{"v":1,"type":"layout","request":1,"action":"split","direction":"left"}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"layout","request":1,"action":"focus-direction","direction":"right","pane":1}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"layout","request":1,"action":"focus-pane","pane":null}"#,
                InvalidField,
            ),
            (r#"{"v":1,"type":"resync","pane":null}"#, InvalidField),
        ];
        for (source, code) in cases {
            assert_eq!(
                parse_client(source.as_bytes()).unwrap_err().code,
                code,
                "{source}"
            );
        }
    }

    #[test]
    fn server_parser_validates_nested_frame_fields() {
        use AttachErrorCode::*;
        let valid = serde_json::from_slice::<Value>(
            &encode_server(&ServerMessage::Frame(sample_frame(CursorShape::Beam))).unwrap(),
        )
        .unwrap();
        let mutate = |path: &[&str], replacement: Option<Value>| -> String {
            let mut value = valid.clone();
            let (last, parents) = path.split_last().unwrap();
            let mut target = &mut value;
            for key in parents {
                target = match key.parse::<usize>() {
                    Ok(index) => &mut target[index],
                    Err(_) => &mut target[*key],
                };
            }
            match (target, replacement) {
                (Value::Object(map), Some(value)) => {
                    map.insert((*last).into(), value);
                }
                (Value::Object(map), None) => {
                    map.remove(*last);
                }
                (Value::Array(items), Some(value)) => items[last.parse::<usize>().unwrap()] = value,
                _ => unreachable!(),
            }
            value.to_string()
        };
        let cases = [
            (mutate(&["v"], Some(json!(0))), UnknownVersion),
            (mutate(&["type"], Some(json!("snapshot"))), UnknownType),
            (mutate(&["cursor"], None), MissingField),
            (
                mutate(&["cursor", "blink"], Some(json!(true))),
                InvalidField,
            ),
            (
                mutate(&["cursor", "shape"], Some(json!("bar"))),
                InvalidField,
            ),
            (
                mutate(&["modes", "keyboard_flags"], Some(json!(256))),
                InvalidField,
            ),
            (
                mutate(&["modes", "app_cursor"], Some(json!(1))),
                InvalidField,
            ),
            (mutate(&["modes", "alternate_screen"], None), MissingField),
            (
                mutate(
                    &["lines", "0", "runs", "0"],
                    Some(json!(["a", "#fff", "i0", 0])),
                ),
                InvalidField,
            ),
            (
                mutate(
                    &["lines", "0", "runs", "0"],
                    Some(json!(["a", "#gg0000", "i0", 0])),
                ),
                InvalidField,
            ),
            (
                mutate(
                    &["lines", "0", "runs", "0"],
                    Some(json!(["a", "i256", "i0", 0])),
                ),
                InvalidField,
            ),
            (
                mutate(
                    &["lines", "0", "runs", "0"],
                    Some(json!(["a", "n65536", "i0", 0])),
                ),
                InvalidField,
            ),
            (
                mutate(
                    &["lines", "0", "runs", "0"],
                    Some(json!(["a", "x1", "i0", 0])),
                ),
                InvalidField,
            ),
            (
                mutate(
                    &["lines", "0", "runs", "0"],
                    Some(json!(["a", "i-1", "i0", 0])),
                ),
                InvalidField,
            ),
            (
                mutate(
                    &["lines", "0", "runs", "0"],
                    Some(json!(["a", "i0", "i0", 4])),
                ),
                InvalidField,
            ),
            (
                mutate(
                    &["lines", "0", "runs", "0"],
                    Some(json!(["", "i0", "i0", 0])),
                ),
                InvalidField,
            ),
            (
                mutate(&["lines", "0", "runs", "0"], Some(json!(["a", "i0", "i0"]))),
                InvalidField,
            ),
            (mutate(&["lines", "0", "row"], None), MissingField),
            (
                mutate(&["selection", "0", "start"], Some(json!(5))),
                InvalidField,
            ),
            (mutate(&["selection", "0", "end"], None), MissingField),
            (
                mutate(&["decorations", "0", "color"], Some(json!("red"))),
                InvalidField,
            ),
            (
                mutate(&["decorations", "0", "separator_row"], Some(Value::Null)),
                InvalidField,
            ),
            (mutate(&["title"], Some(Value::Null)), InvalidField),
            (mutate(&["full"], None), MissingField),
            (mutate(&["extra"], Some(json!(1))), InvalidField),
            (mutate(&["placements"], None), MissingField),
            (mutate(&["placements"], Some(json!({}))), InvalidField),
            (mutate(&["placements", "0", "image"], None), MissingField),
            (
                mutate(&["placements", "0", "image"], Some(json!(4_294_967_296u64))),
                InvalidField,
            ),
            (
                mutate(&["placements", "0", "row"], Some(json!("1"))),
                InvalidField,
            ),
            (
                mutate(&["placements", "0", "z"], Some(Value::Null)),
                InvalidField,
            ),
            (
                mutate(&["placements", "0", "columns"], Some(json!(0))),
                InvalidField,
            ),
            (
                mutate(&["placements", "0", "source_width"], Some(json!(0))),
                InvalidField,
            ),
            (
                mutate(&["placements", "0", "alpha"], Some(json!(1))),
                InvalidField,
            ),
        ];
        for (source, code) in cases {
            assert_eq!(
                parse_server(source.as_bytes()).unwrap_err().code,
                code,
                "{source}"
            );
        }
    }

    #[test]
    fn server_parser_validates_other_messages() {
        use AttachErrorCode::*;
        let cases = [
            (r#"{"v":1,"type":"welcome"}"#, MissingField),
            (r#"{"v":1,"type":"welcome","session":1}"#, InvalidField),
            (
                r#"{"v":1,"type":"layout","revision":1,"active_tab":0,"tabs":[{"title":"a"}]}"#,
                MissingField,
            ),
            (
                r#"{"v":1,"type":"layout","revision":1,"active_tab":0,"tabs":[{"title":"a","panes":[{"pane":1}]}]}"#,
                MissingField,
            ),
            (
                r#"{"v":1,"type":"layout","revision":1,"active_tab":0,"tabs":{}}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"copy-text","request":1,"text":null}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"layout-result","request":1,"error":false}"#,
                InvalidField,
            ),
            (r#"{"v":1,"type":"error","code":"X"}"#, MissingField),
            (
                r#"{"v":1,"type":"exit","reason":"done","code":0}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"hello","width":1,"height":1,"cell_width":1,"cell_height":1}"#,
                UnknownType,
            ),
            (
                r#"{"v":1,"type":"image","pane":1,"image":2,"width":2}"#,
                MissingField,
            ),
            (
                r#"{"v":1,"type":"image","pane":1,"image":2,"width":2,"height":2,"bytes":15}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"image","pane":1,"image":2,"width":0,"height":2,"bytes":0}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"image","pane":1,"image":-2,"width":1,"height":1,"bytes":4}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"image","pane":1,"image":2,"width":1,"height":1,"bytes":4,"format":"rgba"}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"image-data","pane":1,"image":2,"offset":0,"data":""}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"image-data","pane":1,"image":2,"offset":0,"data":"%%%"}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"image-data","pane":1,"image":2,"offset":null,"data":"AA=="}"#,
                InvalidField,
            ),
            (
                r#"{"v":1,"type":"image-data","pane":1,"image":2,"data":"AA=="}"#,
                MissingField,
            ),
            (r#"{"v":1,"type":"image-removed","pane":1}"#, MissingField),
            (
                r#"{"v":1,"type":"image-removed","pane":1,"image":2,"bytes":4}"#,
                InvalidField,
            ),
        ];
        for (source, code) in cases {
            assert_eq!(
                parse_server(source.as_bytes()).unwrap_err().code,
                code,
                "{source}"
            );
        }
    }

    #[test]
    fn oversized_lines_are_rejected_before_parsing_and_encoding() {
        let mut line = vec![b'{'; MAX_ATTACH_LINE_BYTES + 1];
        assert_eq!(
            parse_client(&line).unwrap_err().code,
            AttachErrorCode::LineTooLarge
        );
        assert_eq!(
            parse_server(&line).unwrap_err().code,
            AttachErrorCode::LineTooLarge
        );
        line.clear();
        let oversized = ClientMessage::Input {
            pane: 1,
            data: vec![0; MAX_ATTACH_LINE_BYTES / 4 * 3],
        };
        assert_eq!(
            encode_client(&oversized).unwrap_err().code,
            AttachErrorCode::LineTooLarge
        );
    }

    #[test]
    fn image_chunks_are_bounded_contiguous_and_rejected_when_oversized() {
        let oversized = ServerMessage::ImageData {
            pane: 1,
            image: 1,
            offset: 0,
            data: vec![7; MAX_IMAGE_CHUNK_BYTES + 1],
        };
        assert_eq!(
            encode_server(&oversized).unwrap_err().code,
            AttachErrorCode::InvalidField
        );
        let line = format!(
            r#"{{"v":1,"type":"image-data","pane":1,"image":1,"offset":0,"data":"{}"}}"#,
            STANDARD.encode(vec![7; MAX_IMAGE_CHUNK_BYTES + 1])
        );
        assert_eq!(
            parse_server(line.as_bytes()).unwrap_err().code,
            AttachErrorCode::InvalidField
        );
        let rgba: Vec<u8> = (0..MAX_IMAGE_CHUNK_BYTES * 2 + 12)
            .map(|index| (index % 251) as u8)
            .collect();
        let chunks: Vec<ServerMessage> = image_data_messages(3, 4, &rgba).collect();
        assert_eq!(chunks.len(), 3);
        let mut joined = Vec::new();
        for message in &chunks {
            let ServerMessage::ImageData {
                pane,
                image,
                offset,
                data,
            } = message
            else {
                panic!("expected image data, got {message:?}");
            };
            assert_eq!((*pane, *image), (3, 4));
            assert_eq!(*offset, joined.len() as u64);
            assert!(!data.is_empty() && data.len() <= MAX_IMAGE_CHUNK_BYTES);
            let line = encode_server(message).unwrap();
            assert_eq!(&parse_server(&line).unwrap(), message);
            joined.extend_from_slice(data);
        }
        assert_eq!(joined, rgba);
        assert_eq!(image_data_messages(3, 4, &[]).count(), 0);
        assert_eq!(
            image_announcement(3, 4, 2, 1),
            ServerMessage::Image {
                pane: 3,
                image: 4,
                width: 2,
                height: 1,
                bytes: 8
            }
        );
    }

    /// Deterministic xorshift generator so property-style tests are reproducible.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % bound
        }
    }

    fn random_color(rng: &mut Rng) -> CellColor {
        match rng.next(3) {
            0 => CellColor::Rgb(rng.next(2) as u8 * 255, 0, rng.next(2) as u8),
            1 => CellColor::Indexed(rng.next(2) as u8),
            _ => CellColor::Named(rng.next(2) as u16 + 256),
        }
    }

    #[test]
    fn line_runs_merge_equal_styles_and_invert_exactly() {
        let cell = |character, bold| VisibleCell {
            character,
            foreground: CellColor::Named(256),
            background: CellColor::Named(257),
            bold,
            inverse: false,
        };
        let cells = vec![cell('a', false), cell('b', false), cell('c', true)];
        let runs = line_runs(&cells);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "ab");
        assert_eq!(runs[1].text, "c");
        assert_eq!(cells_from_runs(&runs), cells);
        assert!(line_runs(&[]).is_empty());

        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let characters = ['a', ' ', '\u{e9}', '\u{4e2d}', '\u{1f600}'];
        for _ in 0..500 {
            let len = rng.next(40) as usize;
            let cells: Vec<VisibleCell> = (0..len)
                .map(|_| VisibleCell {
                    character: characters[rng.next(characters.len() as u64) as usize],
                    foreground: random_color(&mut rng),
                    background: random_color(&mut rng),
                    bold: rng.next(2) == 1,
                    inverse: rng.next(2) == 1,
                })
                .collect();
            let runs = line_runs(&cells);
            assert_eq!(cells_from_runs(&runs), cells);
            assert_eq!(
                runs.iter()
                    .map(|run| run.text.chars().count())
                    .sum::<usize>(),
                len
            );
            for pair in runs.windows(2) {
                assert!(
                    !pair[0].same_style(&pair[1]),
                    "unmerged neighbours {pair:?}"
                );
            }
        }
    }

    #[test]
    fn selection_spans_group_cells_and_invert_exactly() {
        let spans = selection_spans(&[(4, 1), (0, 0), (1, 0), (3, 0), (5, 1), (1, 0)]);
        assert_eq!(
            spans,
            vec![
                SelectionSpan {
                    row: 0,
                    start: 0,
                    end: 1
                },
                SelectionSpan {
                    row: 0,
                    start: 3,
                    end: 3
                },
                SelectionSpan {
                    row: 1,
                    start: 4,
                    end: 5
                },
            ]
        );
        assert_eq!(
            selection_cells(&spans),
            vec![(0, 0), (1, 0), (3, 0), (4, 1), (5, 1)]
        );
        assert!(selection_spans(&[]).is_empty());

        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..500 {
            let mut cells: Vec<(usize, usize)> = (0..rng.next(30))
                .map(|_| (rng.next(12) as usize, rng.next(4) as usize))
                .collect();
            let spans = selection_spans(&cells);
            cells.sort_by_key(|&(column, row)| (row, column));
            cells.dedup();
            assert_eq!(selection_cells(&spans), cells);
            assert_eq!(selection_spans(&selection_cells(&spans)), spans);
        }
    }
}

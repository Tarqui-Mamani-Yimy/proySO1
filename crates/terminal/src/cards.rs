//! Block cards of `nulix up`: each shell-integration block in the viewport is
//! drawn as a rounded card behind its text (`card.wgsl`), with its status
//! (`12ms ✓`, `0.4s ✗ 101`, `● 3.1s`) right-aligned on the prompt row.
//!
//! The rows come from [`nulix::renderer::project_block_cards`]; this module
//! holds the pure pixel geometry, the state → style mapping, the meta text,
//! the duration format and the card vertices, so all of it is unit tested
//! without a GPU.

use std::time::{Duration, SystemTime};

pub use crate::chrome::{ACCENT, BORDER, FAILED_BORDER, FAILURE, MUTED, SUCCESS, SURFACE};
use nulix::blocks::BlockState;
use nulix::layout::Rect;
use nulix::renderer::{BlockCard, BlockDecorationColor};

/// Corner radius of a card in pixels.
pub const CARD_RADIUS: f32 = 6.0;
/// Border width of a running or selected card.
const STRONG_BORDER_PX: f32 = 2.0;

/// Formats a command duration: `NNms` under a second, `N.Ns` (truncated to
/// tenths) under a minute, `MmSSs` from there on.
pub fn format_duration(duration: Duration) -> String {
    let ms = duration.as_millis();
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}.{}s", ms / 1_000, ms % 1_000 / 100)
    } else {
        let secs = ms / 1_000;
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// Colours and border width of one card.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CardStyle {
    pub fill: [u8; 3],
    pub border: [u8; 3],
    pub border_px: f32,
}

/// Maps the block's decoration colour to its card: failures get a red-tinted
/// border, a running command (or the block selected with Ctrl+Shift+A) a 2px
/// accent border, everything else the plain border.
pub fn card_style(card: &BlockCard, selected: bool) -> CardStyle {
    let running = card.color == BlockDecorationColor::Accent && card.state == BlockState::Running;
    let (border, border_px) = if selected || running {
        (ACCENT, STRONG_BORDER_PX)
    } else if card.color == BlockDecorationColor::Failure {
        (FAILED_BORDER, 1.0)
    } else {
        (BORDER, 1.0)
    };
    CardStyle {
        fill: SURFACE,
        border,
        border_px,
    }
}

/// One coloured piece of a card's meta text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetaPart {
    pub text: String,
    pub color: [u8; 3],
}

fn part(text: impl Into<String>, color: [u8; 3]) -> MetaPart {
    MetaPart {
        text: text.into(),
        color,
    }
}

/// The meta text of a card: `<duration> ✓`, `<duration> ✗ <code>`, or
/// `● <elapsed>` while running (elapsed up to `now`, so it only advances when
/// the pane redraws). Prompts and cancelled lines have none.
pub fn card_meta(card: &BlockCard, now: SystemTime) -> Vec<MetaPart> {
    let since = |end: SystemTime| {
        card.times
            .started
            .and_then(|start| end.duration_since(start).ok())
            .map(format_duration)
    };
    let mut parts = Vec::new();
    match card.state {
        BlockState::Running => {
            parts.push(part("●", ACCENT));
            if let Some(elapsed) = since(now) {
                parts.push(part(format!(" {elapsed}"), MUTED));
            }
        }
        BlockState::Finished => {
            let duration = card.times.finished.and_then(since);
            let mark = match card.exit_status {
                Some(0) => Some(part("✓", SUCCESS)),
                Some(code) => Some(part(format!("✗ {code}"), FAILURE)),
                None => None,
            };
            match (duration, mark) {
                (Some(duration), Some(mark)) => {
                    parts.push(part(format!("{duration} "), MUTED));
                    parts.push(mark);
                }
                (Some(duration), None) => parts.push(part(duration, MUTED)),
                (None, Some(mark)) => parts.push(mark),
                (None, None) => {}
            }
        }
        BlockState::Prompt | BlockState::Input | BlockState::Cancelled => {}
    }
    parts
}

/// Display width of meta parts in cell columns.
pub fn meta_width(parts: &[MetaPart]) -> usize {
    parts.iter().map(|part| part.text.chars().count()).sum()
}

/// Column where a meta text `width` columns wide starts on the prompt `row`:
/// right-aligned with one column of padding, and `None` when it would not
/// leave a blank column after the row's text.
pub fn meta_column(row: &[char], width: usize) -> Option<usize> {
    let start = row.len().checked_sub(width + 1)?;
    match row.iter().rposition(|character| *character != ' ') {
        Some(last) if start < last + 2 => None,
        _ => Some(start),
    }
}

/// Drops blank rows from the bottom of a card that ends in the viewport
/// (the end mark of a cancelled or implicitly finished command lands after
/// the blank separator row). The prompt row always stays.
pub fn trim_blank_tail(mut card: BlockCard, row_is_blank: impl Fn(usize) -> bool) -> BlockCard {
    // A running block's blank rows include the cursor row it writes to next.
    if card.clipped_bottom || card.state == BlockState::Running {
        return card;
    }
    while card.bottom_row > card.top_row && row_is_blank(card.bottom_row) {
        card.bottom_row -= 1;
    }
    card
}

/// A card's rectangle in window pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CardRect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

/// The card around `card`'s rows of the text `grid`: as tall as its rows and
/// as wide as the grid plus `inset - 1` pixels each side, so the border sits
/// in the grid padding (see `chrome::pane_grid_layout`). A clipped edge is
/// pushed past the grid so the scissor cuts it straight, without a corner.
pub fn card_rect(card: &BlockCard, grid: Rect, cell_height: f32, inset: u32) -> CardRect {
    let margin = inset.saturating_sub(1) as f32;
    let overflow = CARD_RADIUS + STRONG_BORDER_PX + 1.0;
    let top = grid.y as f32 + card.top_row as f32 * cell_height;
    let bottom = grid.y as f32 + (card.bottom_row + 1) as f32 * cell_height;
    CardRect {
        x0: grid.x as f32 - margin,
        y0: if card.clipped_top {
            top - overflow
        } else {
            top
        },
        x1: grid.right() as f32 + margin,
        y1: if card.clipped_bottom {
            bottom + overflow
        } else {
            bottom
        },
    }
}

/// Vertex of the rounded-rectangle card pipeline (`card.wgsl`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CardVertex {
    /// Clip-space position.
    pub position: [f32; 2],
    /// Pixel offset from the card centre.
    pub local: [f32; 2],
    /// Half the card size in pixels.
    pub half_size: [f32; 2],
    /// Corner radius and border width in pixels.
    pub shape: [f32; 2],
    /// Linear fill colour.
    pub fill: [f32; 3],
    /// Linear border colour.
    pub border: [f32; 3],
}

/// Appends the two triangles of one card to `vertices`.
pub fn push_card_vertices(
    vertices: &mut Vec<CardVertex>,
    rect: CardRect,
    style: CardStyle,
    viewport_width: f32,
    viewport_height: f32,
) {
    if viewport_width <= 0.0 || viewport_height <= 0.0 || rect.x1 <= rect.x0 || rect.y1 <= rect.y0 {
        return;
    }
    let half = [(rect.x1 - rect.x0) / 2.0, (rect.y1 - rect.y0) / 2.0];
    let corners = [
        (rect.x0, rect.y0, -1.0, -1.0),
        (rect.x1, rect.y0, 1.0, -1.0),
        (rect.x1, rect.y1, 1.0, 1.0),
        (rect.x0, rect.y1, -1.0, 1.0),
    ];
    let fill = style.fill.map(nulix::renderer::srgb_to_linear);
    let border = style.border.map(nulix::renderer::srgb_to_linear);
    for index in [0, 1, 2, 0, 2, 3] {
        let (x, y, sx, sy) = corners[index];
        vertices.push(CardVertex {
            position: [
                x / viewport_width * 2.0 - 1.0,
                1.0 - y / viewport_height * 2.0,
            ],
            local: [sx * half[0], sy * half[1]],
            half_size: half,
            shape: [CARD_RADIUS, style.border_px],
            fill,
            border,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulix::blocks::{BlockState, BlockTimes};
    use nulix::layout::Rect;
    use nulix::renderer::{BlockCard, BlockDecorationColor};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn card(state: BlockState, color: BlockDecorationColor, exit: Option<i32>) -> BlockCard {
        BlockCard {
            block_id: 1,
            top_row: 2,
            bottom_row: 4,
            clipped_top: false,
            clipped_bottom: false,
            prompt_row: Some(2),
            color,
            state,
            exit_status: exit,
            times: BlockTimes::default(),
        }
    }

    fn at(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    fn timed(mut card: BlockCard, started: u64, finished: Option<u64>) -> BlockCard {
        card.times = BlockTimes {
            started: Some(at(started)),
            finished: finished.map(at),
        };
        card
    }

    fn text(parts: &[MetaPart]) -> String {
        parts.iter().map(|part| part.text.as_str()).collect()
    }

    #[test]
    fn durations_use_ms_then_tenths_then_minutes() {
        assert_eq!(format_duration(Duration::from_millis(0)), "0ms");
        assert_eq!(format_duration(Duration::from_millis(12)), "12ms");
        assert_eq!(format_duration(Duration::from_millis(999)), "999ms");
        assert_eq!(format_duration(Duration::from_millis(1000)), "1.0s");
        assert_eq!(format_duration(Duration::from_millis(3_140)), "3.1s");
        assert_eq!(format_duration(Duration::from_millis(59_999)), "59.9s");
        assert_eq!(format_duration(Duration::from_millis(60_000)), "1m00s");
        assert_eq!(format_duration(Duration::from_millis(125_900)), "2m05s");
    }

    #[test]
    fn card_styles_follow_the_block_decoration_colors() {
        let ok = card_style(
            &card(BlockState::Finished, BlockDecorationColor::Success, Some(0)),
            false,
        );
        assert_eq!(
            ok,
            CardStyle {
                fill: SURFACE,
                border: BORDER,
                border_px: 1.0
            }
        );
        let failed = card_style(
            &card(BlockState::Finished, BlockDecorationColor::Failure, Some(1)),
            false,
        );
        assert_eq!((failed.border, failed.border_px), (FAILED_BORDER, 1.0));
        let running = card_style(
            &card(BlockState::Running, BlockDecorationColor::Accent, None),
            false,
        );
        assert_eq!((running.border, running.border_px), (ACCENT, 2.0));
        let prompt = card_style(
            &card(BlockState::Input, BlockDecorationColor::Accent, None),
            false,
        );
        assert_eq!((prompt.border, prompt.border_px), (BORDER, 1.0));
        let cancelled = card_style(
            &card(BlockState::Cancelled, BlockDecorationColor::Muted, None),
            false,
        );
        assert_eq!((cancelled.border, cancelled.border_px), (BORDER, 1.0));
        let selected = card_style(
            &card(BlockState::Finished, BlockDecorationColor::Failure, Some(1)),
            true,
        );
        assert_eq!((selected.border, selected.border_px), (ACCENT, 2.0));
    }

    #[test]
    fn meta_reports_duration_and_exit_status() {
        let ok = timed(
            card(BlockState::Finished, BlockDecorationColor::Success, Some(0)),
            1_000,
            Some(1_012),
        );
        let parts = card_meta(&ok, at(9_000));
        assert_eq!(text(&parts), "12ms ✓");
        assert_eq!(parts.last().unwrap().color, SUCCESS);

        let failed = timed(
            card(
                BlockState::Finished,
                BlockDecorationColor::Failure,
                Some(101),
            ),
            1_000,
            Some(2_400),
        );
        let parts = card_meta(&failed, at(9_000));
        assert_eq!(text(&parts), "1.4s ✗ 101");
        assert_eq!(parts.last().unwrap().color, FAILURE);
        assert_eq!(parts[0].color, MUTED);

        let running = timed(
            card(BlockState::Running, BlockDecorationColor::Accent, None),
            1_000,
            None,
        );
        let parts = card_meta(&running, at(4_100));
        assert_eq!(text(&parts), "● 3.1s");
        assert_eq!(parts[0].color, ACCENT);

        let cancelled = card(BlockState::Cancelled, BlockDecorationColor::Muted, None);
        assert!(card_meta(&cancelled, at(0)).is_empty());
        let prompt = card(BlockState::Input, BlockDecorationColor::Accent, None);
        assert!(card_meta(&prompt, at(0)).is_empty());
        // A finished block without observed times still shows its status.
        let untimed = card(BlockState::Finished, BlockDecorationColor::Failure, Some(2));
        assert_eq!(text(&card_meta(&untimed, at(0))), "✗ 2");
    }

    #[test]
    fn meta_is_right_aligned_and_omitted_when_it_would_touch_the_prompt() {
        let row: Vec<char> = "$ ls".chars().chain(std::iter::repeat_n(' ', 16)).collect();
        // 20 columns, 6-wide meta, one column of right padding: starts at 13.
        assert_eq!(meta_column(&row, 6), Some(13));
        let long: Vec<char> = "$ echo something long".chars().take(20).collect();
        assert_eq!(meta_column(&long, 6), None);
        // One blank column must separate the prompt text from the meta.
        let tight: Vec<char> = "$ abcdefghij"
            .chars()
            .chain(std::iter::repeat_n(' ', 8))
            .collect();
        assert_eq!(meta_column(&tight, 6), Some(13));
        let touching: Vec<char> = "$ abcdefghijk"
            .chars()
            .chain(std::iter::repeat_n(' ', 7))
            .collect();
        assert_eq!(meta_column(&touching, 6), None);
        assert_eq!(meta_column(&[' '; 4], 6), None);
    }

    #[test]
    fn blank_trailing_rows_are_trimmed_from_unclipped_cards() {
        let base = card(BlockState::Finished, BlockDecorationColor::Success, Some(0));
        let blank = |row: usize| row >= 3;
        assert_eq!(trim_blank_tail(base, blank).bottom_row, 2);
        let clipped = BlockCard {
            clipped_bottom: true,
            ..base
        };
        assert_eq!(trim_blank_tail(clipped, blank).bottom_row, 4);
        assert_eq!(trim_blank_tail(base, |_| true).bottom_row, 2);
        // A running block keeps its blank rows: the cursor row belongs to it.
        let running = card(BlockState::Running, BlockDecorationColor::Accent, None);
        assert_eq!(trim_blank_tail(running, blank).bottom_row, 4);
    }

    #[test]
    fn card_rects_sit_just_outside_the_text_cells() {
        let grid = Rect::new(105, 20, 790, 560);
        let base = card(BlockState::Finished, BlockDecorationColor::Success, Some(0));
        let rect = card_rect(&base, grid, 20.0, 5);
        assert_eq!(
            rect,
            CardRect {
                x0: 101.0,
                y0: 60.0,
                x1: 899.0,
                y1: 120.0
            }
        );
        // Clipped edges continue past the grid so no corner is drawn there.
        let clipped = BlockCard {
            clipped_top: true,
            clipped_bottom: true,
            ..base
        };
        let rect = card_rect(&clipped, grid, 20.0, 5);
        assert!(rect.y0 < 60.0 - CARD_RADIUS && rect.y1 > 120.0 + CARD_RADIUS);
    }

    #[test]
    fn card_vertices_form_one_quad_with_local_pixel_coordinates() {
        let mut vertices = Vec::new();
        let rect = CardRect {
            x0: 0.0,
            y0: 0.0,
            x1: 100.0,
            y1: 50.0,
        };
        let style = CardStyle {
            fill: SURFACE,
            border: BORDER,
            border_px: 1.0,
        };
        push_card_vertices(&mut vertices, rect, style, 200.0, 100.0);
        assert_eq!(vertices.len(), 6);
        assert_eq!(vertices[0].position, [-1.0, 1.0]);
        assert_eq!(vertices[0].local, [-50.0, -25.0]);
        assert_eq!(vertices[2].position, [0.0, 0.0]);
        assert_eq!(vertices[2].local, [50.0, 25.0]);
        assert!(
            vertices
                .iter()
                .all(|v| v.half_size == [50.0, 25.0] && v.shape == [CARD_RADIUS, 1.0])
        );
        push_card_vertices(&mut vertices, rect, style, 0.0, 100.0);
        assert_eq!(vertices.len(), 6);
    }
}

//! Window chrome of the nulix window: the tab bar and, in up mode (`nulix up`),
//! the bottom hint bar.
//!
//! Everything here is pure geometry and text layout so it can be unit tested
//! without a GPU: bar heights per [`UiMode`], the pane content rectangle left
//! between the bars, the up-mode tab bar (tab slots with a `×` close button, a
//! `+` new-tab button and the right-aligned `◆ nulix` label), tab-bar
//! hit-testing, and the hint bar with the focused pane's state. Positions are
//! whole cell columns; the renderer multiplies them by the cell width.

use nulix::{blocks::PaneActivity, layout::Rect};

/// Which chrome the standalone window draws.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiMode {
    /// Plain `nulix` and `nulix --session`: tab bar only with several tabs.
    Classic,
    /// `nulix up`: always-visible tab bar plus the bottom hint bar.
    Up,
}

/// Up-mode bar background.
pub const CHROME: [u8; 3] = [0x0F, 0x11, 0x16];
/// Up-mode active tab background (the terminal background).
pub const ACTIVE_TAB: [u8; 3] = [0x14, 0x16, 0x1C];
/// Up-mode primary chrome text.
pub const TEXT: [u8; 3] = [0xDC, 0xDC, 0xDC];
/// Up-mode muted chrome text (inactive tabs, hint descriptions).
pub const MUTED: [u8; 3] = [0x8A, 0x90, 0xA0];
/// Up-mode borders and separators.
pub const BORDER: [u8; 3] = [0x2A, 0x2F, 0x3A];
/// Up-mode accent (active tab underline, running dot, brand mark).
pub const ACCENT: [u8; 3] = [0x69, 0xA5, 0xDC];
/// Height in pixels of the accent line under the active tab.
pub const ACCENT_LINE_PX: f32 = 2.0;

/// Right-aligned label of the up-mode tab bar.
pub const BRAND_MARK: &str = "◆";
pub const BRAND_NAME: &str = "nulix";

/// Shortcut hints of the bottom bar, in display order: (Linux key, macOS key, description).
/// The macOS keys follow the existing macOS bindings (block output copy stays Ctrl+Shift+O).
// Spelled out (not ⌃⇧) because common monospace fonts lack U+21E7 and a
// first-time viewer reads "Ctrl+Shift" faster. F1 comes first so the help
// hint is the last one dropped on a narrow window.
const HINTS: [(&str, &str, &str); 5] = [
    ("F1", "F1", "ayuda"),
    ("Ctrl+Shift+D", "Cmd+Shift+D", "dividir"),
    ("Ctrl+Shift+T", "Cmd+Shift+T", "tab"),
    ("Ctrl+Shift+A", "Cmd+Shift+A", "bloque"),
    ("Ctrl+Shift+O", "Ctrl+Shift+O", "copiar"),
];

/// Colour role of a chrome text segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tone {
    Text,
    Muted,
    Accent,
}

impl Tone {
    pub fn color(self) -> [u8; 3] {
        match self {
            Self::Text => TEXT,
            Self::Muted => MUTED,
            Self::Accent => ACCENT,
        }
    }
}

/// One piece of chrome text starting at cell column `col`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Segment {
    pub col: u32,
    pub text: String,
    pub tone: Tone,
}

fn bar_row(cell_height: f32) -> u32 {
    cell_height.ceil().max(1.0) as u32
}

/// Height of the top tab bar: classic shows it only with several tabs.
pub fn top_bar_height(mode: UiMode, tab_count: usize, cell_height: f32) -> u32 {
    match mode {
        UiMode::Classic if tab_count <= 1 => 0,
        _ => bar_row(cell_height),
    }
}

/// Height of the bottom hint bar (up mode only).
pub fn bottom_bar_height(mode: UiMode, cell_height: f32) -> u32 {
    match mode {
        UiMode::Classic => 0,
        UiMode::Up => bar_row(cell_height),
    }
}

/// The pane area left between the top and bottom bars.
pub fn content_rect(mode: UiMode, window: Rect, tab_count: usize, cell_height: f32) -> Rect {
    let top = top_bar_height(mode, tab_count, cell_height).min(window.height);
    let rest = window.height - top;
    let bottom = bottom_bar_height(mode, cell_height).min(rest);
    Rect::new(
        window.x,
        window.y.saturating_add(top),
        window.width,
        rest - bottom,
    )
}

/// One tab of the up-mode tab bar, covering columns `start..end`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TabSlot {
    pub start: u32,
    pub end: u32,
    /// The label as drawn (possibly truncated with `…`).
    pub label: String,
    /// Column of the `×` close button.
    pub close_col: u32,
}

/// Column layout of the up-mode tab bar.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpTabBar {
    pub tabs: Vec<TabSlot>,
    /// Columns `start..end` of the `+` button, when it fits.
    pub plus: Option<(u32, u32)>,
    /// Column of the `◆ nulix` label, when it fits.
    pub brand_col: Option<u32>,
}

/// Columns of a tab slot around its label: padding, space, `×`, padding.
const TAB_EXTRA: u32 = 4;
/// Columns of the `+` button (` + `).
const PLUS_WIDTH: u32 = 3;
/// Columns of `◆ nulix` plus one column of right padding.
const BRAND_WIDTH: u32 = 8;

fn truncate_label(label: &str, max: usize) -> String {
    if label.chars().count() <= max {
        return label.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut text: String = label.chars().take(max - 1).collect();
    text.push('…');
    text
}

/// Largest per-label length cap with which the tabs and `+` fit in `available` columns.
fn label_cap(lengths: &[usize], available: u32) -> Option<usize> {
    let longest = lengths.iter().copied().max().unwrap_or(0);
    (0..=longest).rev().find(|&cap| {
        let tabs: u64 = lengths
            .iter()
            .map(|&len| len.min(cap) as u64 + u64::from(TAB_EXTRA))
            .sum();
        tabs + u64::from(PLUS_WIDTH) <= u64::from(available)
    })
}

/// Lays out the up-mode tab bar in `columns` cell columns. Tabs are sized to
/// their labels; when space runs out the labels shrink (keeping the
/// `◆ nulix` label while every label keeps at least one character), then the
/// label is dropped, and tabs that still cannot fit are left out, so nothing
/// ever overflows the bar.
pub fn up_tab_bar(labels: &[&str], columns: u32) -> UpTabBar {
    let lengths: Vec<usize> = labels.iter().map(|label| label.chars().count()).collect();
    let min_cap = usize::from(lengths.iter().any(|&len| len > 0));
    let with_brand = label_cap(&lengths, columns.saturating_sub(BRAND_WIDTH))
        .filter(|&cap| cap >= min_cap && columns >= BRAND_WIDTH);
    let (cap, brand) = match with_brand {
        Some(cap) => (cap, true),
        None => (label_cap(&lengths, columns).unwrap_or(0), false),
    };
    let mut bar = UpTabBar::default();
    let mut col = 0u32;
    for label in labels {
        let label = truncate_label(label, cap);
        let width = label.chars().count() as u32 + TAB_EXTRA;
        if col + width > columns {
            break;
        }
        bar.tabs.push(TabSlot {
            start: col,
            end: col + width,
            close_col: col + width - 2,
            label,
        });
        col += width;
    }
    if col + PLUS_WIDTH <= columns {
        bar.plus = Some((col, col + PLUS_WIDTH));
    }
    if brand {
        bar.brand_col = Some(columns - BRAND_WIDTH);
    }
    bar
}

/// What a click on the up-mode tab bar does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabBarHit {
    Select(usize),
    Close(usize),
    New,
}

/// Hit-tests cell column `column` of the up-mode tab bar.
pub fn up_tab_bar_hit(bar: &UpTabBar, column: u32) -> Option<TabBarHit> {
    if let Some((index, slot)) = bar
        .tabs
        .iter()
        .enumerate()
        .find(|(_, slot)| (slot.start..slot.end).contains(&column))
    {
        return Some(if column >= slot.close_col {
            TabBarHit::Close(index)
        } else {
            TabBarHit::Select(index)
        });
    }
    bar.plus
        .filter(|(start, end)| (*start..*end).contains(&column))
        .map(|_| TabBarHit::New)
}

/// Cell column under pixel `x`, or `None` outside the bar.
pub fn column_at(x: f64, cell_width: f32, width: u32) -> Option<u32> {
    if !x.is_finite() || x < 0.0 || x >= f64::from(width) || cell_width <= 0.0 {
        return None;
    }
    Some((x / f64::from(cell_width)).floor() as u32)
}

/// Dot and label of the focused pane's state: only a running command counts
/// as busy; a pane without shell integration reads as ready.
pub fn state_label(activity: PaneActivity) -> (&'static str, &'static str) {
    match activity {
        PaneActivity::Running => ("●", "ejecutando"),
        PaneActivity::Idle | PaneActivity::Unknown => ("○", "listo"),
    }
}

fn width(text: &str) -> u32 {
    text.chars().count() as u32
}

/// Lays out the bottom hint bar in `columns` cell columns: the pane state on
/// the left, shortcut hints right-aligned. Hints are dropped from the right
/// end until they fit; segments that cannot fit are left out.
pub fn hint_bar(columns: u32, activity: PaneActivity, macos: bool) -> Vec<Segment> {
    let (dot, label) = state_label(activity);
    let running = activity == PaneActivity::Running;
    let mut segments = Vec::new();
    let state = [
        (1, dot, if running { Tone::Accent } else { Tone::Muted }),
        (3, label, if running { Tone::Text } else { Tone::Muted }),
    ];
    for (col, text, tone) in state {
        if col + width(text) <= columns {
            segments.push(Segment {
                col,
                text: text.to_owned(),
                tone,
            });
        }
    }
    let state_end = 3 + width(label);
    let hints: Vec<(&str, &str)> = HINTS
        .iter()
        .map(|(linux, mac, desc)| (if macos { *mac } else { *linux }, *desc))
        .collect();
    let total = |count: usize| -> u32 {
        hints[..count]
            .iter()
            .map(|(key, desc)| width(key) + 1 + width(desc))
            .sum::<u32>()
            + 2 * count.saturating_sub(1) as u32
    };
    let Some(count) = (1..=hints.len())
        .rev()
        .find(|&count| state_end + 2 + total(count) < columns)
    else {
        return segments;
    };
    let mut col = columns - 1 - total(count);
    for (key, desc) in &hints[..count] {
        segments.push(Segment {
            col,
            text: (*key).to_owned(),
            tone: Tone::Text,
        });
        col += width(key) + 1;
        segments.push(Segment {
            col,
            text: (*desc).to_owned(),
            tone: Tone::Muted,
        });
        col += width(desc) + 2;
    }
    segments
}

/// Tab to re-select after closing tab `closed` while tab `active` is
/// selected: `None` when the active tab itself closes (the tab set picks its
/// neighbour), otherwise the previously active tab at its shifted index.
pub fn active_after_closing(closed: usize, active: usize) -> Option<usize> {
    match closed.cmp(&active) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Less => Some(active - 1),
        std::cmp::Ordering::Greater => Some(active),
    }
}

/// A filled chrome rectangle in window pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayRect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub color: [u8; 3],
}

/// A chrome text run in window pixels, clipped to `clip` (left, top, right, bottom).
#[derive(Clone, Debug, PartialEq)]
pub struct OverlayText {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub clip: [i32; 4],
    pub text: String,
    pub color: [u8; 3],
}

/// What the up-mode chrome needs to draw one frame.
pub struct UpChrome<'a> {
    pub labels: &'a [String],
    pub active: usize,
    pub activity: PaneActivity,
    pub macos: bool,
    pub width: u32,
    pub height: u32,
    pub cell_width: f32,
    pub cell_height: f32,
}

/// Rectangles and text runs of the up-mode tab bar and hint bar.
pub fn up_overlay(chrome: &UpChrome<'_>) -> (Vec<OverlayRect>, Vec<OverlayText>) {
    let full = chrome.width as f32;
    let cw = chrome.cell_width;
    let columns = if cw > 0.0 {
        (full / cw).floor() as u32
    } else {
        0
    };
    let top_h =
        top_bar_height(UiMode::Up, chrome.labels.len(), chrome.cell_height).min(chrome.height);
    let bottom_h = bottom_bar_height(UiMode::Up, chrome.cell_height).min(chrome.height - top_h);
    let bottom_y = (chrome.height - bottom_h) as f32;
    let (top, bottom) = (top_h as f32, bottom_h as f32);
    let mut rects = Vec::new();
    let mut texts = Vec::new();
    let mut rect = |x0: f32, y0: f32, x1: f32, y1: f32, color| {
        rects.push(OverlayRect {
            x0,
            y0,
            x1: x1.min(full),
            y1,
            color,
        });
    };
    let mut text = |col: u32, top_y: f32, bar: f32, text: &str, color| {
        texts.push(OverlayText {
            left: col as f32 * cw,
            top: top_y,
            width: (width(text) + 1) as f32 * cw,
            height: bar,
            clip: [0, top_y as i32, chrome.width as i32, (top_y + bar) as i32],
            text: text.to_owned(),
            color,
        });
    };
    if top_h > 0 {
        let labels: Vec<&str> = chrome.labels.iter().map(String::as_str).collect();
        let bar = up_tab_bar(&labels, columns);
        rect(0.0, 0.0, full, top, CHROME);
        rect(0.0, top - 1.0, full, top, BORDER);
        for (index, slot) in bar.tabs.iter().enumerate() {
            let (x0, x1) = (slot.start as f32 * cw, slot.end as f32 * cw);
            let active = index == chrome.active;
            if active {
                rect(x0, 0.0, x1, top, ACTIVE_TAB);
                rect(x0, (top - ACCENT_LINE_PX).max(0.0), x1, top, ACCENT);
            }
            rect(x1 - 1.0, 0.0, x1, top, BORDER);
            let tone = if active { TEXT } else { MUTED };
            text(slot.start + 1, 0.0, top, &slot.label, tone);
            text(slot.close_col, 0.0, top, "×", MUTED);
        }
        if let Some((start, _)) = bar.plus {
            text(start + 1, 0.0, top, "+", TEXT);
        }
        if let Some(col) = bar.brand_col {
            text(col, 0.0, top, BRAND_MARK, ACCENT);
            text(col + 2, 0.0, top, BRAND_NAME, MUTED);
        }
    }
    if bottom_h > 0 {
        rect(0.0, bottom_y, full, bottom_y + bottom, CHROME);
        rect(0.0, bottom_y, full, bottom_y + 1.0, BORDER);
        for segment in hint_bar(columns, chrome.activity, chrome.macos) {
            text(
                segment.col,
                bottom_y,
                bottom,
                &segment.text,
                segment.tone.color(),
            );
        }
    }
    (rects, texts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_bars_match_the_previous_tab_bar_rule() {
        assert_eq!(top_bar_height(UiMode::Classic, 1, 20.0), 0);
        assert_eq!(top_bar_height(UiMode::Classic, 2, 20.0), 20);
        assert_eq!(bottom_bar_height(UiMode::Classic, 20.0), 0);
        let window = Rect::new(0, 0, 800, 600);
        assert_eq!(content_rect(UiMode::Classic, window, 1, 20.0), window);
        assert_eq!(
            content_rect(UiMode::Classic, window, 3, 20.0),
            Rect::new(0, 20, 800, 580)
        );
    }

    #[test]
    fn up_bars_take_one_cell_row_each_even_with_one_tab() {
        assert_eq!(top_bar_height(UiMode::Up, 1, 19.2), 20);
        assert_eq!(top_bar_height(UiMode::Up, 4, 20.0), 20);
        assert_eq!(bottom_bar_height(UiMode::Up, 19.2), 20);
        let window = Rect::new(0, 0, 800, 600);
        assert_eq!(
            content_rect(UiMode::Up, window, 1, 20.0),
            Rect::new(0, 20, 800, 560)
        );
        assert_eq!(
            content_rect(UiMode::Up, Rect::new(0, 0, 800, 30), 1, 20.0),
            Rect::new(0, 20, 800, 0)
        );
        assert_eq!(
            content_rect(UiMode::Up, Rect::new(0, 0, 800, 10), 1, 20.0),
            Rect::new(0, 10, 800, 0)
        );
    }

    #[test]
    fn up_tab_bar_sizes_tabs_to_labels_with_close_plus_and_brand() {
        let bar = up_tab_bar(&["src", "docs"], 80);
        assert_eq!(
            bar.tabs,
            vec![
                TabSlot {
                    start: 0,
                    end: 7,
                    label: "src".into(),
                    close_col: 5
                },
                TabSlot {
                    start: 7,
                    end: 15,
                    label: "docs".into(),
                    close_col: 13
                },
            ]
        );
        assert_eq!(bar.plus, Some((15, 18)));
        assert_eq!(bar.brand_col, Some(72));
    }

    #[test]
    fn up_tab_bar_truncates_labels_before_overflowing() {
        // 2 tabs of 10-char labels: natural width 2*14 + 3 + 8 = 39 columns.
        let labels = ["abcdefghij", "klmnopqrst"];
        let bar = up_tab_bar(&labels, 30);
        assert_eq!(bar.brand_col, Some(22));
        assert!(bar.tabs.iter().all(|slot| slot.label.ends_with('…')));
        let (_, plus_end) = bar.plus.unwrap();
        assert!(plus_end <= 22);
        // Too narrow for the brand: it is dropped, tabs and + still fit.
        let bar = up_tab_bar(&labels, 14);
        assert_eq!(bar.brand_col, None);
        assert_eq!(bar.tabs.len(), 2);
        assert!(bar.plus.unwrap().1 <= 14);
        // Far too narrow: whatever is laid out stays inside the bar.
        for columns in 0..40 {
            let bar = up_tab_bar(&labels, columns);
            assert!(bar.tabs.iter().all(|slot| slot.end <= columns));
            assert!(bar.plus.is_none_or(|(_, end)| end <= columns));
            assert!(bar.brand_col.is_none_or(|col| col + 7 <= columns));
        }
    }

    #[test]
    fn up_tab_bar_hits_select_close_and_new() {
        let bar = up_tab_bar(&["src", "docs"], 80);
        assert_eq!(up_tab_bar_hit(&bar, 0), Some(TabBarHit::Select(0)));
        assert_eq!(up_tab_bar_hit(&bar, 4), Some(TabBarHit::Select(0)));
        assert_eq!(up_tab_bar_hit(&bar, 5), Some(TabBarHit::Close(0)));
        assert_eq!(up_tab_bar_hit(&bar, 7), Some(TabBarHit::Select(1)));
        assert_eq!(up_tab_bar_hit(&bar, 13), Some(TabBarHit::Close(1)));
        assert_eq!(up_tab_bar_hit(&bar, 16), Some(TabBarHit::New));
        assert_eq!(up_tab_bar_hit(&bar, 30), None);
        assert_eq!(up_tab_bar_hit(&bar, 74), None);
        assert_eq!(column_at(25.0, 10.0, 800), Some(2));
        assert_eq!(column_at(-1.0, 10.0, 800), None);
        assert_eq!(column_at(800.0, 10.0, 800), None);
    }

    #[test]
    fn state_label_reads_running_only_while_a_command_runs() {
        assert_eq!(state_label(PaneActivity::Running), ("●", "ejecutando"));
        assert_eq!(state_label(PaneActivity::Idle), ("○", "listo"));
        assert_eq!(state_label(PaneActivity::Unknown), ("○", "listo"));
    }

    fn text_of(segments: &[Segment]) -> Vec<&str> {
        segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    }

    #[test]
    fn hint_bar_shows_state_and_right_aligned_hints() {
        let segments = hint_bar(110, PaneActivity::Running, false);
        assert_eq!(
            text_of(&segments),
            vec![
                "●",
                "ejecutando",
                "F1",
                "ayuda",
                "Ctrl+Shift+D",
                "dividir",
                "Ctrl+Shift+T",
                "tab",
                "Ctrl+Shift+A",
                "bloque",
                "Ctrl+Shift+O",
                "copiar"
            ]
        );
        assert_eq!(segments[0].tone, Tone::Accent);
        assert_eq!(segments[2].tone, Tone::Text);
        assert_eq!(segments[3].tone, Tone::Muted);
        let last = segments.last().unwrap();
        assert_eq!(last.col + width(&last.text), 109);
        let idle = hint_bar(110, PaneActivity::Unknown, true);
        assert_eq!(
            text_of(&idle)[..6],
            ["○", "listo", "F1", "ayuda", "Cmd+Shift+D", "dividir"]
        );
        assert_eq!(idle[0].tone, Tone::Muted);
    }

    #[test]
    fn hint_bar_drops_hints_from_the_right_end_and_never_overflows() {
        let segments = hint_bar(43, PaneActivity::Idle, false);
        assert_eq!(
            text_of(&segments),
            vec!["○", "listo", "F1", "ayuda", "Ctrl+Shift+D", "dividir"]
        );
        assert_eq!(
            text_of(&hint_bar(10, PaneActivity::Idle, false)),
            ["○", "listo"]
        );
        for columns in 0..120 {
            for segment in hint_bar(columns, PaneActivity::Running, false) {
                assert!(segment.col + width(&segment.text) <= columns);
            }
        }
    }
    #[test]
    fn closing_an_inactive_tab_keeps_the_active_tab_selected() {
        assert_eq!(active_after_closing(2, 2), None);
        assert_eq!(active_after_closing(0, 2), Some(1));
        assert_eq!(active_after_closing(3, 2), Some(2));
    }

    #[test]
    fn up_overlay_draws_both_bars_and_underlines_the_active_tab() {
        let labels = vec!["src".to_owned(), "docs".to_owned()];
        let (rects, texts) = up_overlay(&UpChrome {
            labels: &labels,
            active: 1,
            activity: PaneActivity::Running,
            macos: false,
            width: 800,
            height: 600,
            cell_width: 10.0,
            cell_height: 20.0,
        });
        let has = |x0: f32, y0: f32, x1: f32, y1: f32, color: [u8; 3]| {
            rects.contains(&OverlayRect {
                x0,
                y0,
                x1,
                y1,
                color,
            })
        };
        assert!(has(0.0, 0.0, 800.0, 20.0, CHROME));
        assert!(has(70.0, 0.0, 150.0, 20.0, ACTIVE_TAB));
        assert!(has(70.0, 18.0, 150.0, 20.0, ACCENT));
        assert!(has(0.0, 580.0, 800.0, 600.0, CHROME));
        assert!(has(0.0, 580.0, 800.0, 581.0, BORDER));
        assert!(rects.iter().all(|rect| rect.y1 <= 20.0 || rect.y0 >= 580.0));
        let find = |text: &str| texts.iter().find(|item| item.text == text).unwrap();
        assert_eq!((find("src").left, find("src").color), (10.0, MUTED));
        assert_eq!((find("docs").left, find("docs").color), (80.0, TEXT));
        assert_eq!(find("+").left, 160.0);
        assert_eq!((find("◆").left, find("◆").color), (720.0, ACCENT));
        assert_eq!((find("ejecutando").top, find("●").color), (580.0, ACCENT));
    }
}

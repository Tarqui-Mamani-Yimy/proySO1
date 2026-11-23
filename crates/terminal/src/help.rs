//! F1 help overlay of `nulix up`: a centred panel listing the window's
//! shortcuts over a dark scrim.
//!
//! Everything here is pure so it can be unit tested without a GPU: the key
//! routing decision (who gets F1, Esc and every other key while the help is
//! open), the shortcut list (the real bindings of `main.rs`, with the macOS
//! variants), and the panel layout in window pixels (size, centring, and
//! shrinking with clipped text on small windows). The renderer draws the scrim
//! and the panel with the rounded-card pipeline (`cards.rs`) and the text with
//! the chrome text path.

use crate::chrome::{self, ACCENT, MUTED, OverlayText, TEXT, UiMode};
use nulix::keyboard::Modifiers;
use winit::keyboard::{Key, NamedKey};

/// Colour of the scrim drawn over the whole window behind the panel.
pub const SCRIM: [u8; 3] = [8, 9, 12];
/// Opacity of the scrim.
pub const SCRIM_OPACITY: f32 = 0.62;
/// Border of the help panel (fill is [`chrome::SURFACE`]).
pub const PANEL_BORDER: [u8; 3] = [0x3A, 0x41, 0x50];
/// Shortcut descriptions.
pub const DESCRIPTION: [u8; 3] = [0xC4, 0xC8, 0xD2];
/// Title of the panel.
pub const TITLE: &str = "Atajos de nulix";
/// Smallest gap in pixels left between the panel and the window edges.
pub const MARGIN_PX: f32 = 16.0;

const PAD_COLS: u32 = 2;
const PAD_ROWS: u32 = 1;
/// Columns between a description and its keys.
const ENTRY_GAP: u32 = 2;
/// Columns between the two group columns.
const COLUMN_GAP: u32 = 4;
/// Groups of the left and right column, by index into [`help_groups`].
const LEFT_GROUPS: [usize; 3] = [0, 1, 3];
const RIGHT_GROUPS: [usize; 2] = [2, 4];

/// What a key press means to the help overlay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelpKey {
    /// F1 without modifiers.
    F1,
    Escape,
    Other,
}

/// What the window does with a key press.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelpRoute {
    /// Open the help; the key is consumed.
    Open,
    /// Close the help; the key is consumed.
    Close,
    /// The help is open: the key is consumed and does nothing.
    Swallow,
    /// Normal handling: shortcuts, then the focused pane's PTY.
    PassThrough,
}

/// Classifies a pressed key for [`route_help_key`].
pub fn help_key(key: &Key, mods: Modifiers) -> HelpKey {
    let modified = mods.control || mods.alt || mods.shift || mods.super_key;
    match key {
        Key::Named(NamedKey::F1) if !modified => HelpKey::F1,
        Key::Named(NamedKey::Escape) => HelpKey::Escape,
        _ => HelpKey::Other,
    }
}

/// The help exists only in up mode. While it is open, F1 or Esc closes it and
/// every other key is swallowed; while it is closed, F1 opens it unless the
/// focused pane shows the alternate screen (TUI apps use F1 themselves).
pub fn route_help_key(mode: UiMode, open: bool, alternate_screen: bool, key: HelpKey) -> HelpRoute {
    match (mode, open, key) {
        (UiMode::Classic, _, _) => HelpRoute::PassThrough,
        (UiMode::Up, true, HelpKey::F1 | HelpKey::Escape) => HelpRoute::Close,
        (UiMode::Up, true, HelpKey::Other) => HelpRoute::Swallow,
        (UiMode::Up, false, HelpKey::F1) if !alternate_screen => HelpRoute::Open,
        (UiMode::Up, false, _) => HelpRoute::PassThrough,
    }
}

/// One shortcut: what it does and the keys (or command) that do it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpEntry {
    pub label: &'static str,
    pub keys: String,
}

/// A titled group of shortcuts. `commands` groups list shell commands, drawn
/// in the accent colour.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpGroup {
    pub name: &'static str,
    pub commands: bool,
    pub entries: Vec<HelpEntry>,
}

fn entry(label: &'static str, keys: impl Into<String>) -> HelpEntry {
    HelpEntry {
        label,
        keys: keys.into(),
    }
}

/// The shortcuts the window implements (`pane_action`, `tab_shortcut`,
/// `block_shortcut` and the clipboard keys in `main.rs`), with the macOS
/// variants those functions accept. Keys are spelled out because common
/// monospace fonts lack the ⇧ glyph.
pub fn help_groups(macos: bool) -> Vec<HelpGroup> {
    let (cmd, opt) = if macos {
        ("Cmd", "Option")
    } else {
        ("Ctrl", "Alt")
    };
    let group = |name, entries| HelpGroup {
        name,
        commands: false,
        entries,
    };
    vec![
        group(
            "Paneles",
            vec![
                entry("dividir →", format!("{cmd}+Shift+D")),
                entry("dividir ↓", format!("{cmd}+Shift+E")),
                entry("mover foco", format!("{cmd}+{opt}+Flechas")),
                entry("cerrar", format!("{cmd}+Shift+W")),
            ],
        ),
        group(
            "Tabs",
            vec![
                entry("nueva", format!("{cmd}+Shift+T")),
                entry("siguiente", "Ctrl+Tab"),
                entry("anterior", "Ctrl+Shift+Tab"),
                entry("ir a tab", if macos { "Cmd+1..9" } else { "Alt+1..9" }),
                entry("cerrar", format!("{cmd}+Shift+Q")),
            ],
        ),
        group(
            "Bloques",
            vec![
                entry(
                    "navegar",
                    if macos {
                        "Cmd+↑/↓"
                    } else {
                        "Ctrl+Shift+↑/↓"
                    },
                ),
                entry("seleccionar", format!("{cmd}+Shift+A")),
                // Block copy keys are Ctrl+Shift on macOS too.
                entry("copiar salida", "Ctrl+Shift+O"),
                entry("copiar comando", "Ctrl+Shift+L"),
            ],
        ),
        group(
            "Copiar",
            vec![
                entry("copiar", if macos { "Cmd+C" } else { "Ctrl+Shift+C" }),
                entry("pegar", if macos { "Cmd+V" } else { "Ctrl+Shift+V" }),
            ],
        ),
        HelpGroup {
            name: "Sesión",
            commands: true,
            entries: vec![
                entry("paneles y estado", "nulixctl list"),
                entry("esperar un comando", "nulixctl wait"),
                entry("sesión persistente", "nulix --session NAME"),
            ],
        },
    ]
}

/// The panel rectangle in window pixels and its text runs, clipped to the
/// inside of the panel border.
#[derive(Clone, Debug, PartialEq)]
pub struct HelpLayout {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub texts: Vec<OverlayText>,
}

/// Label and key column widths of one group column, and its height in rows.
fn column_size(groups: &[HelpGroup], indices: &[usize]) -> (u32, u32, u32) {
    let mut label = 0;
    let mut keys = 0;
    let mut rows = 0;
    for (n, group) in indices.iter().filter_map(|&i| groups.get(i)).enumerate() {
        label = label.max(chrome::width(&group.name.to_uppercase()));
        for item in &group.entries {
            label = label.max(chrome::width(item.label));
            keys = keys.max(chrome::width(&item.keys));
        }
        rows += u32::from(n > 0) + 1 + group.entries.len() as u32;
    }
    (label, keys, rows)
}

/// Lays the help out centred in a `width` x `height` window. The natural size
/// fits the content; on a smaller window the panel shrinks to the window less
/// [`MARGIN_PX`] on each side and its text is clipped, never overflowing.
pub fn help_layout(
    groups: &[HelpGroup],
    width: u32,
    height: u32,
    cell_width: f32,
    cell_height: f32,
) -> HelpLayout {
    let (cw, ch) = (cell_width.max(1.0), cell_height.max(1.0));
    let left = column_size(groups, &LEFT_GROUPS);
    let right = column_size(groups, &RIGHT_GROUPS);
    let left_width = left.0 + ENTRY_GAP + left.1;
    let right_width = right.0 + ENTRY_GAP + right.1;
    let close = "Esc cerrar";
    let header = chrome::width(TITLE) + COLUMN_GAP + chrome::width(close);
    let inner = (left_width + COLUMN_GAP + right_width).max(header);
    let natural_cols = PAD_COLS * 2 + inner;
    // Title, blank row, then the group columns.
    let natural_rows = PAD_ROWS * 2 + 2 + left.2.max(right.2);
    let (window_w, window_h) = (width as f32, height as f32);
    let panel_w = (natural_cols as f32 * cw)
        .min(window_w - 2.0 * MARGIN_PX)
        .max(0.0)
        .floor();
    let panel_h = (natural_rows as f32 * ch)
        .min(window_h - 2.0 * MARGIN_PX)
        .max(0.0)
        .floor();
    let x0 = ((window_w - panel_w) / 2.0).floor().max(0.0);
    let y0 = ((window_h - panel_h) / 2.0).floor().max(0.0);
    let (x1, y1) = (x0 + panel_w, y0 + panel_h);
    let clip = [
        (x0 + 1.0) as i32,
        (y0 + 1.0) as i32,
        (x1 - 1.0).max(x0) as i32,
        (y1 - 1.0).max(y0) as i32,
    ];
    let mut texts = Vec::new();
    let mut text = |col: u32, row: u32, text: &str, color: [u8; 3]| {
        texts.push(OverlayText {
            left: x0 + col as f32 * cw,
            top: y0 + row as f32 * ch,
            width: (chrome::width(text) + 1) as f32 * cw,
            height: ch,
            clip,
            text: text.to_owned(),
            color,
        });
    };
    // "Esc cerrar" follows the panel's right edge, after the title at least.
    let panel_cols = (panel_w / cw).floor() as u32;
    let close_col = panel_cols
        .saturating_sub(PAD_COLS + chrome::width(close))
        .max(PAD_COLS + chrome::width(TITLE) + COLUMN_GAP);
    text(PAD_COLS, PAD_ROWS, TITLE, TEXT);
    text(close_col, PAD_ROWS, "Esc", TEXT);
    text(close_col + 4, PAD_ROWS, "cerrar", MUTED);
    let columns = [
        (PAD_COLS, &LEFT_GROUPS[..], left.0),
        (
            PAD_COLS + left_width + COLUMN_GAP,
            &RIGHT_GROUPS[..],
            right.0,
        ),
    ];
    for (col, indices, label_width) in columns {
        let mut row = PAD_ROWS + 2;
        for (n, group) in indices.iter().filter_map(|&i| groups.get(i)).enumerate() {
            row += u32::from(n > 0);
            text(col, row, &group.name.to_uppercase(), MUTED);
            row += 1;
            let key_color = if group.commands { ACCENT } else { TEXT };
            for item in &group.entries {
                text(col, row, item.label, DESCRIPTION);
                text(col + label_width + ENTRY_GAP, row, &item.keys, key_color);
                row += 1;
            }
        }
    }
    HelpLayout {
        x0,
        y0,
        x1,
        y1,
        texts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(control: bool, shift: bool, alt: bool) -> Modifiers {
        Modifiers {
            control,
            shift,
            alt,
            ..Modifiers::default()
        }
    }

    #[test]
    fn f1_and_escape_are_classified_and_modified_f1_is_other() {
        let f1 = Key::Named(NamedKey::F1);
        assert_eq!(help_key(&f1, Modifiers::default()), HelpKey::F1);
        assert_eq!(help_key(&f1, mods(true, false, false)), HelpKey::Other);
        assert_eq!(help_key(&f1, mods(false, true, false)), HelpKey::Other);
        assert_eq!(
            help_key(&Key::Named(NamedKey::Escape), Modifiers::default()),
            HelpKey::Escape
        );
        assert_eq!(
            help_key(&Key::Character("a".into()), Modifiers::default()),
            HelpKey::Other
        );
    }

    #[test]
    fn classic_mode_routes_f1_to_the_pty() {
        for alternate in [false, true] {
            for key in [HelpKey::F1, HelpKey::Escape, HelpKey::Other] {
                assert_eq!(
                    route_help_key(UiMode::Classic, false, alternate, key),
                    HelpRoute::PassThrough
                );
            }
        }
    }

    #[test]
    fn up_mode_f1_opens_the_help_unless_the_alternate_screen_is_active() {
        assert_eq!(
            route_help_key(UiMode::Up, false, false, HelpKey::F1),
            HelpRoute::Open
        );
        assert_eq!(
            route_help_key(UiMode::Up, false, true, HelpKey::F1),
            HelpRoute::PassThrough
        );
        assert_eq!(
            route_help_key(UiMode::Up, false, false, HelpKey::Escape),
            HelpRoute::PassThrough
        );
        assert_eq!(
            route_help_key(UiMode::Up, false, false, HelpKey::Other),
            HelpRoute::PassThrough
        );
    }

    #[test]
    fn open_help_closes_on_f1_or_escape_and_swallows_other_keys() {
        for alternate in [false, true] {
            assert_eq!(
                route_help_key(UiMode::Up, true, alternate, HelpKey::F1),
                HelpRoute::Close
            );
            assert_eq!(
                route_help_key(UiMode::Up, true, alternate, HelpKey::Escape),
                HelpRoute::Close
            );
            assert_eq!(
                route_help_key(UiMode::Up, true, alternate, HelpKey::Other),
                HelpRoute::Swallow
            );
        }
    }

    fn flat(groups: &[HelpGroup]) -> Vec<(&str, &str, &str)> {
        groups
            .iter()
            .flat_map(|group| {
                group
                    .entries
                    .iter()
                    .map(move |item| (group.name, item.label, item.keys.as_str()))
            })
            .collect()
    }

    #[test]
    fn help_lists_the_linux_bindings_by_group() {
        let groups = help_groups(false);
        let names: Vec<_> = groups.iter().map(|group| group.name).collect();
        assert_eq!(names, ["Paneles", "Tabs", "Bloques", "Copiar", "Sesión"]);
        assert_eq!(
            flat(&groups),
            [
                ("Paneles", "dividir →", "Ctrl+Shift+D"),
                ("Paneles", "dividir ↓", "Ctrl+Shift+E"),
                ("Paneles", "mover foco", "Ctrl+Alt+Flechas"),
                ("Paneles", "cerrar", "Ctrl+Shift+W"),
                ("Tabs", "nueva", "Ctrl+Shift+T"),
                ("Tabs", "siguiente", "Ctrl+Tab"),
                ("Tabs", "anterior", "Ctrl+Shift+Tab"),
                ("Tabs", "ir a tab", "Alt+1..9"),
                ("Tabs", "cerrar", "Ctrl+Shift+Q"),
                ("Bloques", "navegar", "Ctrl+Shift+↑/↓"),
                ("Bloques", "seleccionar", "Ctrl+Shift+A"),
                ("Bloques", "copiar salida", "Ctrl+Shift+O"),
                ("Bloques", "copiar comando", "Ctrl+Shift+L"),
                ("Copiar", "copiar", "Ctrl+Shift+C"),
                ("Copiar", "pegar", "Ctrl+Shift+V"),
                ("Sesión", "paneles y estado", "nulixctl list"),
                ("Sesión", "esperar un comando", "nulixctl wait"),
                ("Sesión", "sesión persistente", "nulix --session NAME"),
            ]
        );
        let commands: Vec<_> = groups.iter().map(|group| group.commands).collect();
        assert_eq!(commands, [false, false, false, false, true]);
    }

    #[test]
    fn help_lists_the_macos_variants_the_bindings_accept() {
        let groups = help_groups(true);
        let keys: Vec<_> = flat(&groups).into_iter().map(|(_, _, keys)| keys).collect();
        assert_eq!(
            keys,
            [
                "Cmd+Shift+D",
                "Cmd+Shift+E",
                "Cmd+Option+Flechas",
                "Cmd+Shift+W",
                "Cmd+Shift+T",
                "Ctrl+Tab",
                "Ctrl+Shift+Tab",
                "Cmd+1..9",
                "Cmd+Shift+Q",
                "Cmd+↑/↓",
                "Cmd+Shift+A",
                "Ctrl+Shift+O",
                "Ctrl+Shift+L",
                "Cmd+C",
                "Cmd+V",
                "nulixctl list",
                "nulixctl wait",
                "nulix --session NAME",
            ]
        );
    }

    fn text<'a>(layout: &'a HelpLayout, value: &str) -> &'a OverlayText {
        layout
            .texts
            .iter()
            .find(|item| item.text == value)
            .unwrap_or_else(|| panic!("missing text {value:?}"))
    }

    #[test]
    fn panel_is_centred_at_its_natural_size_on_a_large_window() {
        let layout = help_layout(&help_groups(false), 1600, 1000, 10.0, 20.0);
        // Left column: "mover foco" (10) + 2 + "Ctrl+Alt+Flechas" (16) = 28;
        // right column: "sesión persistente" (18) + 2 + "nulix --session
        // NAME" (20) = 40; 2 + 28 + 4 + 40 + 2 = 76 columns.
        assert_eq!(layout.x1 - layout.x0, 760.0);
        // 1 + title + blank + left column (3 names + 11 entries + 2 gaps) + 1.
        assert_eq!(layout.y1 - layout.y0, 20.0 * 20.0);
        assert_eq!(layout.x0, 420.0);
        assert_eq!(layout.y0, 300.0);
        let title = text(&layout, TITLE);
        assert_eq!((title.left, title.top), (440.0, 320.0));
        assert_eq!(title.color, TEXT);
        let close = text(&layout, "cerrar");
        // "cerrar" ends at the right padding.
        assert_eq!(close.left + 6.0 * 10.0, layout.x1 - 2.0 * 10.0);
        assert_eq!(close.color, MUTED);
        assert_eq!(text(&layout, "PANELES").color, MUTED);
        assert_eq!(text(&layout, "SESIÓN").color, MUTED);
        assert_eq!(text(&layout, "Ctrl+Shift+D").color, TEXT);
        assert_eq!(text(&layout, "dividir →").color, DESCRIPTION);
        assert_eq!(text(&layout, "nulixctl list").color, ACCENT);
        let clip = [421, 301, 1179, 699];
        assert!(layout.texts.iter().all(|item| item.clip == clip));
    }

    #[test]
    fn panel_shrinks_to_a_small_window_with_a_margin_and_clips_its_text() {
        let layout = help_layout(&help_groups(false), 400, 300, 10.0, 20.0);
        assert_eq!(
            (layout.x0, layout.y0, layout.x1, layout.y1),
            (16.0, 16.0, 384.0, 284.0)
        );
        for item in &layout.texts {
            assert_eq!(item.clip, [17, 17, 383, 283]);
        }
        // Esc cerrar never collides with the title, even when clipped.
        let title_end = text(&layout, TITLE).left + 15.0 * 10.0;
        assert!(text(&layout, "Esc").left > title_end);
    }

    #[test]
    fn panel_is_empty_on_a_window_smaller_than_its_margins() {
        let layout = help_layout(&help_groups(false), 20, 10, 10.0, 20.0);
        assert!(layout.x1 <= layout.x0 && layout.y1 <= layout.y0);
        assert!(layout.x0 >= 0.0 && layout.x1 <= 20.0);
        assert!(layout.y0 >= 0.0 && layout.y1 <= 10.0);
    }
}

//! Renderer-independent grid layout and terminal color conversion.
use crate::model::{CellColor, VisibleCell};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderCell {
    pub character: char,
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub bold: bool,
    pub cursor: bool,
}

pub fn grid_size(width: u32, height: u32, cell_width: f32, cell_height: f32) -> (usize, usize) {
    let cols = (width as f32 / cell_width.max(1.0)).floor() as usize;
    let rows = (height as f32 / cell_height.max(1.0)).floor() as usize;
    (cols.max(1), rows.max(1))
}

pub fn color_rgb(color: CellColor) -> [u8; 3] {
    match color {
        CellColor::Rgb(r, g, b) => [r, g, b],
        CellColor::Named(index) => match index {
            0..=15 => ANSI[index as usize],
            256 => [220, 220, 220],
            257 => [20, 22, 28],
            258 => [255, 255, 255],
            259..=266 => ANSI[(index - 259) as usize],
            267 => [220, 220, 220],
            _ => [220, 220, 220],
        },
        CellColor::Indexed(index) => indexed_rgb(index),
    }
}

pub fn srgb_to_linear(channel: u8) -> f32 {
    let value = f32::from(channel) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn indexed_rgb(index: u8) -> [u8; 3] {
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        0..=15 => ANSI[index as usize],
        16..=231 => {
            let i = index - 16;
            [
                CUBE[(i / 36) as usize],
                CUBE[((i / 6) % 6) as usize],
                CUBE[(i % 6) as usize],
            ]
        }
        _ => {
            let gray = 8 + 10 * (index - 232);
            [gray, gray, gray]
        }
    }
}

const ANSI: [[u8; 3]; 16] = [
    [0, 0, 0],
    [205, 49, 49],
    [13, 188, 121],
    [229, 229, 16],
    [36, 114, 200],
    [188, 63, 188],
    [17, 168, 205],
    [229, 229, 229],
    [102, 102, 102],
    [241, 76, 76],
    [35, 209, 139],
    [245, 245, 67],
    [59, 142, 234],
    [214, 112, 214],
    [41, 184, 219],
    [255, 255, 255],
];

pub fn render_cells(rows: &[Vec<VisibleCell>], cursor: (usize, usize)) -> Vec<RenderCell> {
    rows.iter()
        .enumerate()
        .flat_map(|(row, cells)| {
            cells.iter().enumerate().map(move |(col, cell)| {
                let active = (col, row) == cursor;
                let inverse = cell.inverse ^ active;
                RenderCell {
                    character: cell.character,
                    foreground: color_rgb(if inverse {
                        cell.background
                    } else {
                        cell.foreground
                    }),
                    background: color_rgb(if inverse {
                        cell.foreground
                    } else {
                        cell.background
                    }),
                    bold: cell.bold,
                    cursor: active,
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_layout_clamps_minimized_and_nonintegral_sizes() {
        assert_eq!(grid_size(0, 0, 8.0, 16.0), (1, 1));
        assert_eq!(grid_size(81, 35, 8.0, 16.0), (10, 2));
    }

    #[test]
    fn indexed_palette_includes_cube_grayscale_and_truecolor() {
        assert_eq!(color_rgb(CellColor::Named(1)), [205, 49, 49]);
        assert_eq!(color_rgb(CellColor::Named(256)), [220, 220, 220]);
        assert_eq!(color_rgb(CellColor::Indexed(196)), [255, 0, 0]);
        assert_eq!(color_rgb(CellColor::Indexed(244)), [128, 128, 128]);
        assert_eq!(color_rgb(CellColor::Rgb(1, 2, 3)), [1, 2, 3]);
    }

    #[test]
    fn cursor_and_inverse_swap_colors_in_snapshot_projection() {
        let rows = vec![vec![VisibleCell {
            character: 'x',
            foreground: CellColor::Rgb(1, 2, 3),
            background: CellColor::Rgb(4, 5, 6),
            bold: true,
            inverse: false,
        }]];
        let cell = render_cells(&rows, (0, 0))[0];
        assert_eq!(cell.foreground, [4, 5, 6]);
        assert_eq!(cell.background, [1, 2, 3]);
        assert!(cell.cursor && cell.bold);
    }

    #[test]
    fn background_channels_are_converted_for_srgb_surface_output() {
        assert!((srgb_to_linear(20) - 0.007).abs() < 0.001);
        assert_eq!(srgb_to_linear(0), 0.0);
        assert_eq!(srgb_to_linear(255), 1.0);
    }
}

//! Renderer-independent grid layout and terminal color conversion.
use crate::model::{CellColor, VisibleCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderCell {
    pub character: char,
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub bold: bool,
    pub cursor: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceFailure {
    Lost,
    Outdated,
    Timeout,
    Occluded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameRecovery {
    RetryAfter(Duration),
    WaitForEvent,
}

pub fn frame_recovery(failure: SurfaceFailure) -> FrameRecovery {
    match failure {
        SurfaceFailure::Lost | SurfaceFailure::Outdated => {
            FrameRecovery::RetryAfter(Duration::from_millis(50))
        }
        SurfaceFailure::Timeout => FrameRecovery::RetryAfter(Duration::from_millis(50)),
        SurfaceFailure::Occluded => FrameRecovery::WaitForEvent,
    }
}

#[derive(Default)]
pub struct SurfaceRetryBackoff {
    next_ms: u64,
}
impl SurfaceRetryBackoff {
    pub fn next_delay(&mut self) -> Duration {
        let delay = if self.next_ms == 0 { 50 } else { self.next_ms };
        self.next_ms = (delay * 2).min(1000);
        Duration::from_millis(delay)
    }
    pub fn reset(&mut self) {
        self.next_ms = 0;
    }
}

#[derive(Default)]
pub struct WheelAccumulator {
    remainder: f64,
}
impl WheelAccumulator {
    pub fn push_lines(&mut self, lines: f64) -> i32 {
        let total = self.remainder + lines;
        let whole = if total >= 0.0 {
            (total + 1e-9).floor()
        } else {
            (total - 1e-9).ceil()
        }
        .clamp(i32::MIN as f64, i32::MAX as f64) as i32;
        self.remainder = total - f64::from(whole);
        whole
    }
}

pub fn scaled_font_size(font_size: f32, scale_factor: f64) -> f32 {
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor as f32
    } else {
        1.0
    };
    (font_size * scale).max(1.0)
}

pub fn unicode_cell_width(character: char) -> usize {
    let code = character as u32;
    if character == '\0'
        || matches!(code, 0x0300..=0x036f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff | 0x20d0..=0x20ff | 0xfe00..=0xfe0f | 0xfe20..=0xfe2f)
    {
        0
    } else if matches!(code, 0x1100..=0x115f | 0x2329..=0x232a | 0x2e80..=0xa4cf | 0xac00..=0xd7a3 | 0xf900..=0xfaff | 0xfe10..=0xfe19 | 0xfe30..=0xfe6f | 0xff00..=0xff60 | 0xffe0..=0xffe6 | 0x1f300..=0x1faff | 0x20000..=0x3fffd)
    {
        2
    } else {
        1
    }
}

pub fn glyph_cell_positions(text: &str, cell_width: f32) -> Vec<f32> {
    let mut column = 0usize;
    text.chars()
        .map(|character| {
            let x = column as f32 * cell_width;
            column += unicode_cell_width(character);
            x
        })
        .collect()
}

pub fn next_vertex_buffer_capacity_bytes(
    current_capacity_bytes: usize,
    required_bytes: usize,
) -> usize {
    if required_bytes <= current_capacity_bytes {
        current_capacity_bytes.max(1)
    } else {
        required_bytes.max(1).next_power_of_two()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledRun {
    pub text: String,
    pub foreground: [u8; 3],
    pub bold: bool,
}

pub fn styled_lines(rows: &[Vec<RenderCell>]) -> Vec<Vec<StyledRun>> {
    rows.iter()
        .map(|row| {
            let mut runs: Vec<StyledRun> = Vec::new();
            for cell in row {
                if let Some(last) = runs
                    .last_mut()
                    .filter(|run| run.foreground == cell.foreground && run.bold == cell.bold)
                {
                    last.text.push(cell.character);
                } else {
                    runs.push(StyledRun {
                        text: cell.character.to_string(),
                        foreground: cell.foreground,
                        bold: cell.bold,
                    });
                }
            }
            runs
        })
        .collect()
}

#[derive(Default)]
pub struct WakeupGate(AtomicBool);
impl WakeupGate {
    /// Returns true only for the first queued wakeup until the UI acknowledges it.
    pub fn request(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }
    pub fn acknowledge(&self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct ExitPollBackoff {
    next_ms: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitPollDecision {
    Exit,
    RetryAt(Instant),
}
impl ExitPollBackoff {
    pub fn new(_now: Instant) -> Self {
        Self { next_ms: 10 }
    }
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next_ms;
        self.next_ms = (self.next_ms * 2).min(500);
        Duration::from_millis(delay)
    }
    pub fn observe_wait_status(&mut self, ready: bool, now: Instant) -> ExitPollDecision {
        if ready {
            ExitPollDecision::Exit
        } else {
            ExitPollDecision::RetryAt(now + self.next_delay())
        }
    }
}

pub fn scaled_cell_metrics(cell_width: f32, cell_height: f32, scale_factor: f64) -> (f32, f32) {
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor as f32
    } else {
        1.0
    };
    (
        (cell_width * scale).max(1.0),
        (cell_height * scale).max(1.0),
    )
}

pub fn project_terminal_metrics(
    logical_font_size: f32,
    logical_cell_width: f32,
    logical_cell_height: f32,
    scale_factor: f64,
) -> (f32, f32, f32) {
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor as f32
    } else {
        1.0
    };
    (
        (logical_font_size * scale).max(1.0),
        (logical_cell_width * scale).max(1.0),
        (logical_cell_height * scale).max(1.0),
    )
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

pub fn surface_clear_color() -> [f64; 4] {
    let [r, g, b] = color_rgb(CellColor::Named(257));
    [
        f64::from(srgb_to_linear(r)),
        f64::from(srgb_to_linear(g)),
        f64::from(srgb_to_linear(b)),
        1.0,
    ]
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

pub fn render_cells(rows: &[Vec<VisibleCell>], cursor: (usize, usize)) -> Vec<Vec<RenderCell>> {
    rows.iter()
        .enumerate()
        .map(|(row, cells)| {
            cells
                .iter()
                .enumerate()
                .map(move |(col, cell)| {
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
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waitpid_not_ready_schedules_bounded_retry_until_exit_is_observed() {
        let start = std::time::Instant::now();
        let mut poll = ExitPollBackoff::new(start);
        assert_eq!(
            poll.observe_wait_status(false, start),
            ExitPollDecision::RetryAt(start + std::time::Duration::from_millis(10))
        );
        let second = start + std::time::Duration::from_millis(10);
        assert_eq!(
            poll.observe_wait_status(false, second),
            ExitPollDecision::RetryAt(second + std::time::Duration::from_millis(20))
        );
        let mut now = second;
        for _ in 0..10 {
            now += poll.next_delay();
        }
        assert_eq!(
            poll.observe_wait_status(false, now),
            ExitPollDecision::RetryAt(now + std::time::Duration::from_millis(500))
        );
        assert_eq!(poll.observe_wait_status(true, now), ExitPollDecision::Exit);
    }

    #[test]
    fn surface_loss_backoff_bounds_reconfigure_redraw_rate() {
        let mut backoff = SurfaceRetryBackoff::default();
        assert_eq!(backoff.next_delay(), Duration::from_millis(50));
        assert_eq!(backoff.next_delay(), Duration::from_millis(100));
        for _ in 0..10 {
            backoff.next_delay();
        }
        assert_eq!(backoff.next_delay(), Duration::from_millis(1000));
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(50));
    }

    #[test]
    fn anchored_glyph_positions_preserve_terminal_cell_columns() {
        let positions = glyph_cell_positions("日a\0□🙂", 8.0);
        assert_eq!(positions, [0.0, 16.0, 24.0, 24.0, 32.0]);
    }

    #[test]
    fn wheel_accumulator_quantizes_across_events_and_preserves_direction() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(wheel.push_lines(0.4), 0);
        assert_eq!(wheel.push_lines(0.4), 0);
        assert_eq!(wheel.push_lines(0.4), 1);
        assert_eq!(wheel.push_lines(-0.6), 0);
        assert_eq!(wheel.push_lines(-0.6), -1);
    }

    #[test]
    fn font_size_scales_with_hidpi_cell_metrics() {
        assert_eq!(scaled_font_size(14.0, 2.0), 28.0);
    }

    #[test]
    fn failed_surface_frame_uses_bounded_retry_policy() {
        assert_eq!(
            frame_recovery(SurfaceFailure::Lost),
            FrameRecovery::RetryAfter(Duration::from_millis(50))
        );
        assert_eq!(
            frame_recovery(SurfaceFailure::Outdated),
            FrameRecovery::RetryAfter(Duration::from_millis(50))
        );
        assert_eq!(
            frame_recovery(SurfaceFailure::Timeout),
            FrameRecovery::RetryAfter(std::time::Duration::from_millis(50))
        );
        assert_eq!(
            frame_recovery(SurfaceFailure::Occluded),
            FrameRecovery::WaitForEvent
        );
    }

    #[test]
    fn line_shaping_groups_spans_by_foreground_and_bold_without_losing_spaces() {
        let cells = vec![
            RenderCell {
                character: 'a',
                foreground: [255, 0, 0],
                background: [0; 3],
                bold: false,
                cursor: false,
            },
            RenderCell {
                character: 'b',
                foreground: [255, 0, 0],
                background: [0; 3],
                bold: false,
                cursor: false,
            },
            RenderCell {
                character: ' ',
                foreground: [255, 0, 0],
                background: [0; 3],
                bold: false,
                cursor: false,
            },
            RenderCell {
                character: 'c',
                foreground: [0, 255, 0],
                background: [0; 3],
                bold: true,
                cursor: false,
            },
        ];
        let lines = styled_lines(&[cells]);
        assert_eq!(
            lines[0]
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>(),
            "ab c"
        );
        assert_eq!(lines[0].len(), 2);
        assert_eq!(lines[0][1].foreground, [0, 255, 0]);
        assert!(lines[0][1].bold);
    }

    #[test]
    fn pty_wakeups_coalesce_until_the_ui_drains_them() {
        let gate = WakeupGate::default();
        assert!(gate.request());
        assert!(!gate.request());
        gate.acknowledge();
        assert!(gate.request());
    }

    #[test]
    fn vertex_capacity_grows_geometrically_and_is_retained() {
        assert_eq!(next_vertex_buffer_capacity_bytes(1024, 700), 1024);
        assert_eq!(next_vertex_buffer_capacity_bytes(1024, 1025), 2048);
        assert_eq!(next_vertex_buffer_capacity_bytes(0, 1), 1);
    }

    #[test]
    fn tiny_grid_and_hidpi_metrics_keep_at_least_one_cell() {
        assert_eq!(grid_size(1, 1, 8.0, 16.0), (1, 1));
        assert_eq!(scaled_cell_metrics(8.0, 16.0, 2.0), (16.0, 32.0));
        let (cw, ch) = scaled_cell_metrics(8.0, 16.0, 1.5);
        assert_eq!(grid_size(1200, 900, cw, ch), (100, 37));
        assert_eq!(scaled_cell_metrics(8.0, 16.0, f64::NAN), (8.0, 16.0));
    }

    #[test]
    fn terminal_metric_projection_scales_logical_measurements_once() {
        assert_eq!(
            project_terminal_metrics(14.0, 8.0, 19.0, 2.0),
            (28.0, 16.0, 38.0)
        );
        assert_eq!(
            project_terminal_metrics(14.0, 8.0, 19.0, 1.0),
            (14.0, 8.0, 19.0)
        );
        assert_eq!(
            project_terminal_metrics(14.0, 8.0, 19.0, f64::NAN),
            (14.0, 8.0, 19.0)
        );
    }

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
    fn surface_clear_matches_default_terminal_cell_background() {
        let model = crate::model::Model::new(1, 1);
        let cell = &model.visible_cells()[0][0];
        let [r, g, b] = color_rgb(cell.background);
        assert_eq!(
            surface_clear_color(),
            [
                f64::from(srgb_to_linear(r)),
                f64::from(srgb_to_linear(g)),
                f64::from(srgb_to_linear(b)),
                1.0,
            ]
        );
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
        let cell = render_cells(&rows, (0, 0))[0][0];
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

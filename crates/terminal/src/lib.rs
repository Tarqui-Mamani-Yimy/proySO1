pub use nulix_core::{
    apc, attach, block_navigation, blocks, graphics, keyboard, layout, model, osc, placements,
    renderer,
};
pub mod config;
pub mod control;
pub mod graphics_pipeline;
pub mod pty;
pub mod remote;
pub mod selection;
pub mod session;

#[cfg(all(test, unix))]
mod model_pty_tests;

/// Everything the GPU renderer needs to draw one pane's grid.
///
/// Both window modes produce it: the standalone window builds it from its local
/// [`model::Model`], and an attached client builds it from the server's frames
/// ([`remote::RemotePane::render_input`]). The renderer only consumes this type,
/// so glyph shaping and cell drawing are shared by both paths.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneRenderInput {
    /// Visible grid rows, top to bottom.
    pub rows: Vec<Vec<model::VisibleCell>>,
    /// Cursor cell as `(column, row)`; `None` draws no cursor.
    pub cursor: Option<(usize, usize)>,
    /// Selected cells as `(column, row)`.
    pub selected: Vec<(usize, usize)>,
    /// Block markers and separators in viewport rows.
    pub decorations: Vec<renderer::BlockDecoration>,
}

impl PaneRenderInput {
    /// Projects a local model exactly as the standalone window always has.
    pub fn from_model(model: &model::Model) -> Self {
        let rows = model.visible_cells();
        let cursor = model.cursor();
        let viewport = renderer::BlockViewport {
            evicted_rows: model.block_row_base(),
            history_size: model.history_size(),
            display_offset: model.display_offset(),
            first_visible_line: Some(model.visible_top_line()),
            screen_lines: rows.len(),
            alternate_screen: model.alternate_screen_active(),
        };
        let decorations = renderer::project_block_decorations(model.blocks(), viewport);
        Self {
            selected: model.selected_cells(),
            cursor: Some((cursor.column, cursor.row)),
            rows,
            decorations,
        }
    }

    /// Resolves colors (with `palette`), the cursor and the selection
    /// highlight into render cells.
    pub fn render_cells(&self, palette: &renderer::Palette) -> Vec<Vec<renderer::RenderCell>> {
        let cursor = self.cursor.unwrap_or((usize::MAX, usize::MAX));
        let mut cells = renderer::render_cells_with_palette(&self.rows, cursor, palette);
        for &(column, row) in &self.selected {
            if let Some(cell) = cells.get_mut(row).and_then(|row| row.get_mut(column)) {
                renderer::apply_selection_highlight(cell);
            }
        }
        cells
    }
}

/// Default interactive-shell command candidates, in preference order.
pub fn default_shell_candidates() -> &'static [&'static str] {
    #[cfg(unix)]
    {
        &["shellux"]
    }
    #[cfg(windows)]
    {
        &["pwsh", "cmd"]
    }
    #[cfg(not(any(unix, windows)))]
    {
        &[]
    }
}

pub fn default_shell_candidates_for(windows: bool) -> &'static [&'static str] {
    if windows {
        &["pwsh", "cmd"]
    } else {
        &["shellux"]
    }
}

/// Pure decision logic; production supplies the environment, sibling existence, and PATH probes.
pub fn resolve_shell_choice<'a>(
    override_path: Option<&'a str>,
    sibling_exists: bool,
    candidates: &'a [&'a str],
) -> &'a str {
    if let Some(path) = override_path {
        return path;
    }
    if sibling_exists {
        return "sibling-shellux";
    }
    candidates.first().copied().unwrap_or("shellux")
}

/// Surface dimensions must be non-zero even when a window is minimized.
pub fn clamp_surface_size(width: u32, height: u32) -> (u32, u32) {
    (width.max(1), height.max(1))
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_surface_size, default_shell_candidates, default_shell_candidates_for,
        resolve_shell_choice,
    };

    #[test]
    fn default_shell_candidates_are_platform_ordered() {
        #[cfg(unix)]
        assert_eq!(default_shell_candidates(), &["shellux"]);
        #[cfg(windows)]
        assert_eq!(default_shell_candidates(), &["pwsh", "cmd"]);
    }

    #[test]
    fn surface_size_never_contains_zero_dimensions() {
        assert_eq!(clamp_surface_size(0, 0), (1, 1));
        assert_eq!(clamp_surface_size(1280, 0), (1280, 1));
        assert_eq!(clamp_surface_size(0, 720), (1, 720));
        assert_eq!(clamp_surface_size(1280, 720), (1280, 720));
    }

    #[test]
    fn shell_selection_prefers_override_then_sibling_then_path_candidates() {
        assert_eq!(
            resolve_shell_choice(Some("custom"), true, &["shellux"]),
            "custom"
        );
        assert_eq!(
            resolve_shell_choice(None, true, &["shellux"]),
            "sibling-shellux"
        );
        assert_eq!(resolve_shell_choice(None, false, &["shellux"]), "shellux");
    }

    #[test]
    fn windows_shell_candidate_order_is_testable_on_any_host() {
        assert_eq!(resolve_shell_choice(None, false, &["pwsh", "cmd"]), "pwsh");
        assert_eq!(default_shell_candidates_for(true), &["pwsh", "cmd"]);
    }

    #[test]
    fn model_render_input_marks_cursor_and_highlights_selected_cells() {
        let mut model = crate::model::Model::new(3, 1);
        model.feed(b"ab");
        model.begin_selection(0, 0, alacritty_terminal::selection::SelectionType::Simple);
        model.extend_selection(0, 0);
        let input = super::PaneRenderInput::from_model(&model);
        assert_eq!(input.cursor, Some((2, 0)));
        assert_eq!(input.selected, model.selected_cells());
        let cells = input.render_cells(&crate::renderer::Palette::default());
        assert!(cells[0][2].cursor);
        let plain = crate::renderer::render_cells(&input.rows, (2, 0));
        for &(column, row) in &input.selected {
            assert_eq!(cells[row][column].foreground, plain[row][column].background);
        }
        let hidden = super::PaneRenderInput {
            cursor: None,
            ..input
        };
        assert!(
            hidden
                .render_cells(&crate::renderer::Palette::default())
                .iter()
                .flatten()
                .all(|cell| !cell.cursor)
        );
    }

    #[test]
    fn headless_model_remains_available_through_terminal_reexport() {
        let mut model = crate::model::Model::new(1, 1);
        model.feed(b"x");
        assert_eq!(model.visible_cells()[0][0].character, 'x');
    }
}

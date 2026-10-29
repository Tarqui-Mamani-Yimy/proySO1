//! Pure block target selection and retained-grid text extraction.

use crate::blocks::{Block, BlockList, GridScreen};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavigationDirection {
    Previous,
    Next,
}

pub fn navigation_target(
    blocks: &BlockList,
    viewport_top: i64,
    direction: NavigationDirection,
) -> Option<i64> {
    let mut prompts: Vec<_> = blocks
        .blocks()
        .iter()
        .filter_map(|block| block.grid_anchors.prompt_start)
        .filter(|anchor| anchor.screen == GridScreen::Primary)
        .map(|anchor| anchor.line)
        .collect();
    prompts.sort_unstable();
    prompts.dedup();
    match direction {
        NavigationDirection::Previous => prompts
            .iter()
            .copied()
            .rfind(|line| *line < viewport_top)
            .or_else(|| prompts.first().copied()),
        NavigationDirection::Next => prompts
            .iter()
            .copied()
            .find(|line| *line > viewport_top)
            .or_else(|| prompts.last().copied()),
    }
}

pub fn block_at_or_under(blocks: &BlockList, focus_line: i64) -> Option<&Block> {
    let anchored = |block: &&Block| {
        block
            .grid_anchors
            .prompt_start
            .filter(|anchor| anchor.screen == GridScreen::Primary)
    };
    blocks
        .blocks()
        .iter()
        .filter_map(|block| anchored(&block).map(|anchor| (anchor.line, block)))
        .filter(|(line, _)| *line >= focus_line)
        .filter(|(_, block)| {
            block.state == crate::blocks::BlockState::Finished
                && block.grid_anchors.output_start.is_some()
                && block.grid_anchors.end.is_some()
        })
        .min_by_key(|(line, _)| *line)
        .map(|(_, block)| block)
        .or_else(|| {
            blocks.blocks().iter().rev().find(|block| {
                block.state == crate::blocks::BlockState::Finished && anchored(block).is_some()
            })
        })
}

/// Extracts a half-open cell range. Soft-wrapped rows are joined; hard line
/// breaks are preserved. Alacritty's NUL spacer cells for wide glyphs are skipped.
pub fn extract_range_text(
    rows: &[Vec<char>],
    soft_wrapped: &[bool],
    start: (usize, usize),
    end: (usize, usize),
) -> Option<String> {
    if start > end || start.0 >= rows.len() || end.0 >= rows.len() {
        return None;
    }
    let mut output = String::new();
    for (row, cells) in rows.iter().enumerate().take(end.0 + 1).skip(start.0) {
        let first = if row == start.0 { start.1 } else { 0 }.min(cells.len());
        let last = if row == end.0 { end.1 } else { cells.len() }.min(cells.len());
        if first > last {
            return None;
        }
        let mut trimmed_last = last;
        while trimmed_last > first && cells[trimmed_last - 1] == ' ' {
            trimmed_last -= 1;
        }
        for character in cells[first..trimmed_last].iter().copied() {
            if character != '\0' {
                output.push(character);
            }
        }
        if row < end.0 && !soft_wrapped.get(row).copied().unwrap_or(false) {
            output.push('\n');
        }
    }
    while output.ends_with('\n') {
        output.pop();
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use crate::blocks::{Block, BlockList, BlockState, GridAnchor, GridAnchors, GridScreen};

    use super::*;

    fn block(id: u64, line: i64) -> Block {
        Block {
            id,
            boundaries: Default::default(),
            exit_status: Some(0),
            cwd: None,
            state: BlockState::Finished,
            grid_anchors: GridAnchors {
                prompt_start: Some(GridAnchor {
                    line,
                    column: 0,
                    screen: GridScreen::Primary,
                }),
                input_start: None,
                output_start: Some(GridAnchor {
                    line: line + 1,
                    column: 2,
                    screen: GridScreen::Primary,
                }),
                end: Some(GridAnchor {
                    line: line + 3,
                    column: 0,
                    screen: GridScreen::Primary,
                }),
            },
        }
    }

    #[test]
    fn navigation_clamps_to_anchored_prompts_and_skips_alternate_screen() {
        let mut list = BlockList::default();
        list.push(block(1, 10));
        list.push(block(2, 20));
        let mut alternate = block(3, 30);
        alternate.grid_anchors.prompt_start.as_mut().unwrap().screen = GridScreen::Alternate;
        list.push(alternate);

        assert_eq!(
            navigation_target(&list, 0, NavigationDirection::Previous),
            Some(10)
        );
        assert_eq!(
            navigation_target(&list, 15, NavigationDirection::Previous),
            Some(10)
        );
        assert_eq!(
            navigation_target(&list, 15, NavigationDirection::Next),
            Some(20)
        );
        assert_eq!(
            navigation_target(&list, 99, NavigationDirection::Next),
            Some(20)
        );
        assert_eq!(
            navigation_target(&BlockList::default(), 0, NavigationDirection::Next),
            None
        );
    }

    #[test]
    fn block_focus_prefers_prompt_at_or_below_focus_then_latest_finished() {
        let mut list = BlockList::default();
        list.push(block(1, 10));
        list.push(block(2, 20));
        assert_eq!(block_at_or_under(&list, 12).map(|block| block.id), Some(2));
        assert_eq!(block_at_or_under(&list, 25).map(|block| block.id), Some(2));
    }

    #[test]
    fn extracts_wrapped_and_wide_text_and_trims_final_newline() {
        let lines = [vec!['a', 'b', '日', '\0'], vec!['c', 'd', ' ', ' ']];
        let text = extract_range_text(&lines, &[true, false], (0, 0), (1, 2)).unwrap();
        assert_eq!(text, "ab日cd");
    }
}

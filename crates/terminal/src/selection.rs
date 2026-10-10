use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionGesture {
    Simple,
    Semantic,
    Lines,
}

#[derive(Default)]
pub struct ClickTracker {
    last: Option<(Instant, (usize, usize), u8)>,
}

impl ClickTracker {
    pub fn click(&mut self, cell: (usize, usize), now: Instant) -> SelectionGesture {
        let count = match self.last {
            Some((last_at, last_cell, count))
                if last_cell == cell
                    && now.saturating_duration_since(last_at) <= Duration::from_millis(500) =>
            {
                count % 3 + 1
            }
            _ => 1,
        };
        self.last = Some((now, cell, count));
        match count {
            2 => SelectionGesture::Semantic,
            3 => SelectionGesture::Lines,
            _ => SelectionGesture::Simple,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_clicks_cycle_word_and_line_selection_only_at_same_cell() {
        let start = Instant::now();
        let mut clicks = ClickTracker::default();
        assert_eq!(clicks.click((4, 2), start), SelectionGesture::Simple);
        assert_eq!(
            clicks.click((4, 2), start + Duration::from_millis(200)),
            SelectionGesture::Semantic
        );
        assert_eq!(
            clicks.click((4, 2), start + Duration::from_millis(300)),
            SelectionGesture::Lines
        );
        assert_eq!(
            clicks.click((5, 2), start + Duration::from_millis(350)),
            SelectionGesture::Simple
        );
    }
}

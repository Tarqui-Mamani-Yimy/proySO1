use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionGesture {
    Simple,
    Semantic,
    Lines,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerAction {
    Begin(SelectionGesture),
    Extend,
    Clear,
    End,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerState {
    Pressed,
    Released,
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

#[derive(Default)]
pub struct SelectionRouter {
    selecting: bool,
    clicks: ClickTracker,
}

impl SelectionRouter {
    pub fn mouse_input(
        &mut self,
        button: PointerButton,
        state: PointerState,
        cell: Option<(usize, usize)>,
        now: Instant,
    ) -> PointerAction {
        match (button, state) {
            (PointerButton::Primary, PointerState::Pressed) => {
                let Some(cell) = cell else {
                    self.selecting = false;
                    return PointerAction::Clear;
                };
                self.selecting = true;
                PointerAction::Begin(self.clicks.click(cell, now))
            }
            (PointerButton::Primary, PointerState::Released) if self.selecting => {
                self.selecting = false;
                PointerAction::End
            }
            (_, PointerState::Pressed) => {
                self.selecting = false;
                PointerAction::Clear
            }
            _ => PointerAction::Ignore,
        }
    }

    pub fn cursor_moved(&self, cell: Option<(usize, usize)>) -> PointerAction {
        if self.selecting && cell.is_some() {
            PointerAction::Extend
        } else {
            PointerAction::Ignore
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

    #[test]
    fn pointer_routing_starts_extends_ends_and_clears_selection() {
        let now = Instant::now();
        let mut router = SelectionRouter::default();
        assert_eq!(
            router.mouse_input(
                PointerButton::Primary,
                PointerState::Pressed,
                Some((2, 1)),
                now
            ),
            PointerAction::Begin(SelectionGesture::Simple)
        );
        assert_eq!(router.cursor_moved(Some((4, 1))), PointerAction::Extend);
        assert_eq!(router.cursor_moved(None), PointerAction::Ignore);
        assert_eq!(
            router.mouse_input(
                PointerButton::Primary,
                PointerState::Released,
                Some((4, 1)),
                now
            ),
            PointerAction::End
        );
        assert_eq!(router.cursor_moved(Some((4, 1))), PointerAction::Ignore);
        assert_eq!(
            router.mouse_input(PointerButton::Other, PointerState::Pressed, None, now),
            PointerAction::Clear
        );
    }
}

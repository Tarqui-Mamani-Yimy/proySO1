use nu_ansi_term::Style;
use reedline::{Hinter, History, SearchQuery};

#[derive(Default)]
pub(crate) struct HistoryHinter {
    current_hint: String,
}

impl Hinter for HistoryHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        history: &dyn History,
        use_ansi_coloring: bool,
        _cwd: &str,
    ) -> String {
        self.current_hint = if !line.is_empty() && pos == line.len() {
            history
                .search(SearchQuery::last_with_prefix(line.to_owned(), None))
                .ok()
                .and_then(|entries| entries.into_iter().next())
                .and_then(|entry| entry.command_line.strip_prefix(line).map(str::to_owned))
                .unwrap_or_default()
        } else {
            String::new()
        };

        if use_ansi_coloring && !self.current_hint.is_empty() {
            Style::new().dimmed().paint(&self.current_hint).to_string()
        } else {
            self.current_hint.clone()
        }
    }

    fn complete_hint(&self) -> String {
        self.current_hint.clone()
    }

    fn next_hint_token(&self) -> String {
        self.current_hint
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::HistoryHinter;
    use reedline::{FileBackedHistory, Hinter, History, HistoryItem};

    #[test]
    fn history_hinter_returns_the_most_recent_matching_suffix() {
        let mut history = FileBackedHistory::new(20).unwrap();
        history
            .save(HistoryItem::from_command_line("echo apple"))
            .unwrap();
        history
            .save(HistoryItem::from_command_line("echo apricot"))
            .unwrap();
        let mut hinter = HistoryHinter::default();

        assert_eq!(hinter.handle("echo a", 6, &history, false, ""), "pricot");
        assert_eq!(hinter.complete_hint(), "pricot");
        assert!(
            hinter
                .handle("echo a", 6, &history, true, "")
                .contains("\x1b[2m")
        );
    }

    #[test]
    fn history_hinter_is_empty_without_a_prefix_match_or_at_an_interior_cursor() {
        let mut history = FileBackedHistory::new(20).unwrap();
        history
            .save(HistoryItem::from_command_line("echo apple"))
            .unwrap();
        let mut hinter = HistoryHinter::default();

        assert_eq!(hinter.handle("printf", 6, &history, false, ""), "");
        assert_eq!(hinter.handle("echo a", 4, &history, false, ""), "");
    }
}

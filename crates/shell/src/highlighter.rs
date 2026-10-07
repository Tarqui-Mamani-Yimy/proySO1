use crate::completion::{
    CompletionContext, CompletionState, command_position, unescape_shell_word,
};
use epsh::lexer::{Lexer, Token};
use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};
use std::ops::Range;

pub(crate) struct ShellHighlighter {
    state: CompletionState,
}

impl ShellHighlighter {
    pub(crate) fn new(state: CompletionState) -> Self {
        Self { state }
    }
}

impl Highlighter for ShellHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        self.state
            .with_context(|context| highlight_line(line, context))
    }
}

pub(crate) fn highlight_line(line: &str, context: &CompletionContext) -> StyledText {
    let mut lexer = Lexer::new(line);
    let mut tokens = Vec::new();
    loop {
        match lexer.next_token() {
            Ok((Token::Eof, _)) => break,
            Ok((token, span)) => tokens.push((token, span.offset)),
            Err(_) => return plain_text(line),
        }
    }

    let chars = line.chars().collect::<Vec<_>>();
    let mut styles = vec![Style::new(); chars.len()];
    let comments = comment_ranges(&chars);
    for index in 0..tokens.len() {
        let (token, start) = &tokens[index];
        let mut end = tokens
            .get(index + 1)
            .map_or(chars.len(), |(_, next_start)| *next_start)
            .min(chars.len());
        while end > *start && matches!(chars[end - 1], ' ' | '\t') {
            end -= 1;
        }
        if let Some(comment) = comments
            .iter()
            .find(|comment| *start < comment.start && comment.start < end && comment.end > *start)
        {
            end = comment.start;
            while end > *start && matches!(chars[end - 1], ' ' | '\t') {
                end -= 1;
            }
        }
        if *start >= end {
            continue;
        }
        let source_word = chars[*start..end].iter().collect::<String>();
        let style = match token {
            Token::Pipe
            | Token::And
            | Token::Or
            | Token::Semi
            | Token::Amp
            | Token::SemiSemi
            | Token::Less
            | Token::Great
            | Token::DLess
            | Token::DGreat
            | Token::LessAnd
            | Token::GreatAnd
            | Token::LessGreat
            | Token::DLessDash
            | Token::Clobber
            | Token::LParen
            | Token::RParen => Some(Style::new().fg(Color::Blue)),
            Token::Word(_, _) if command_position(&line[..char_to_byte(line, *start)]) => {
                let name = unescape_shell_word(&source_word);
                Some(if context.resolves_command(&name) {
                    Style::new().fg(Color::Green)
                } else {
                    Style::new().fg(Color::Red)
                })
            }
            Token::Word(_, _) | Token::Assignment { .. } if source_word.contains(['\'', '"']) => {
                Some(Style::new().fg(Color::Yellow))
            }
            _ => None,
        };
        if let Some(style) = style {
            apply_style(&mut styles, *start..end, style);
        }
    }

    for range in variable_ranges(&chars) {
        apply_style(&mut styles, range, Style::new().fg(Color::Magenta));
    }
    for range in comments {
        apply_style(&mut styles, range, Style::new().dimmed());
    }

    styled_text(&chars, &styles)
}

fn plain_text(line: &str) -> StyledText {
    let mut text = StyledText::new();
    if !line.is_empty() {
        text.push((Style::new(), line.to_owned()));
    }
    text
}

fn char_to_byte(line: &str, char_offset: usize) -> usize {
    line.char_indices()
        .nth(char_offset)
        .map_or(line.len(), |(byte_offset, _)| byte_offset)
}

fn apply_style(styles: &mut [Style], range: Range<usize>, style: Style) {
    for item in styles.get_mut(range).into_iter().flatten() {
        *item = style;
    }
}

fn variable_ranges(chars: &[char]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if quote != Some('\'') && ch == '\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
                index += 1;
                continue;
            }
            if active == '"' && ch == '$' {
                let end = variable_end(chars, index);
                if end > index + 1 {
                    ranges.push(index..end);
                    index = end;
                    continue;
                }
            }
            index += 1;
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '$' => {
                let end = variable_end(chars, index);
                if end > index + 1 {
                    ranges.push(index..end);
                    index = end;
                    continue;
                }
            }
            _ => {}
        }
        index += 1;
    }
    ranges
}

fn variable_end(chars: &[char], start: usize) -> usize {
    let mut index = start + 1;
    let braced = chars.get(index) == Some(&'{');
    if braced {
        index += 1;
    }
    if chars
        .get(index)
        .is_some_and(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
    {
        while chars
            .get(index)
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        {
            index += 1;
        }
    } else if index < chars.len() {
        index += 1;
    }
    if braced && chars.get(index) == Some(&'}') {
        index += 1;
    }
    index
}

fn comment_ranges(chars: &[char]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if quote != Some('\'') && ch == '\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
            index += 1;
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '#' if index == 0
                || chars[index - 1].is_whitespace()
                || matches!(chars[index - 1], ';' | '|' | '&' | '(' | ')' | '<' | '>') =>
            {
                let end = chars[index..]
                    .iter()
                    .position(|ch| *ch == '\n')
                    .map_or(chars.len(), |offset| index + offset);
                ranges.push(index..end);
                index = end;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    ranges
}

fn styled_text(chars: &[char], styles: &[Style]) -> StyledText {
    let mut output = StyledText::new();
    let Some(mut active_style) = styles.first().copied() else {
        return output;
    };
    let mut segment = String::new();
    for (ch, style) in chars.iter().zip(styles.iter().copied()) {
        if style != active_style {
            output.push((active_style, std::mem::take(&mut segment)));
            active_style = style;
        }
        segment.push(*ch);
    }
    if !segment.is_empty() {
        output.push((active_style, segment));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::highlight_line;
    use crate::completion::CompletionContext;
    use epsh::error::ExitStatus;
    use epsh::eval::Shell;
    use reedline::{Color, StyledText};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn context() -> CompletionContext {
        let mut shell = Shell::builder().env_clear().build();
        shell.set_var("PATH", "").unwrap();
        CompletionContext::from_shell(&shell)
    }

    fn style_at(text: &StyledText, source: &str, needle: &str) -> (Option<Color>, bool) {
        let byte_offset = source.find(needle).unwrap();
        let mut offset = 0;
        for (style, segment) in &text.buffer {
            if byte_offset < offset + segment.len() {
                return (style.foreground, style.is_dimmed);
            }
            offset += segment.len();
        }
        panic!("no style found for {needle:?}");
    }

    fn plain_text(text: &StyledText) -> String {
        text.buffer
            .iter()
            .map(|(_, segment)| segment.as_str())
            .collect()
    }

    fn byte_range_at(text: &StyledText, needle: &str) -> std::ops::Range<usize> {
        let mut byte_offset = 0;
        for (_, segment) in &text.buffer {
            if let Some(offset) = segment.find(needle) {
                return byte_offset + offset..byte_offset + offset + needle.len();
            }
            byte_offset += segment.len();
        }
        panic!("no byte range found for {needle:?}");
    }

    #[test]
    fn syntax_highlighter_styles_commands_strings_operators_variables_and_comments() {
        let context = context();
        let source = "echo \"hello $HOME\" | missing-command # comment";
        let styled = highlight_line(source, &context);

        assert_eq!(plain_text(&styled), source);
        assert_eq!(style_at(&styled, source, "echo").0, Some(Color::Green));
        let commented_command = "echo # this comment follows a command";
        let commented = highlight_line(commented_command, &context);
        assert_eq!(
            style_at(&commented, commented_command, "echo").0,
            Some(Color::Green)
        );
        assert_eq!(
            style_at(&styled, source, "missing-command").0,
            Some(Color::Red)
        );
        assert_eq!(style_at(&styled, source, "\"hello").0, Some(Color::Yellow));
        assert_eq!(style_at(&styled, source, "$HOME").0, Some(Color::Magenta));
        assert_eq!(style_at(&styled, source, "|").0, Some(Color::Blue));
        assert!(style_at(&styled, source, "# comment").1);
    }

    #[test]
    fn syntax_highlighter_maps_byte_offsets_after_multibyte_utf8() {
        let context = context();
        let source = "echo 'café'; missing-command";
        let styled = highlight_line(source, &context);

        assert_eq!(plain_text(&styled), source);
        assert_eq!(style_at(&styled, source, "'café'").0, Some(Color::Yellow));
        assert_eq!(style_at(&styled, source, ";").0, Some(Color::Blue));
        assert_eq!(
            style_at(&styled, source, "missing-command").0,
            Some(Color::Red)
        );
        let byte_offset = source.find("missing-command").unwrap();
        assert_eq!(
            byte_range_at(&styled, "missing-command"),
            byte_offset..byte_offset + "missing-command".len()
        );
    }

    #[test]
    fn syntax_highlighter_colors_every_resolved_command_source_green() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let executable = bin.join("path-tool");
        fs::write(&executable, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let mut shell = Shell::builder()
            .cwd(temp.path().to_path_buf())
            .env_clear()
            .build();
        shell.set_var("PATH", bin.to_str().unwrap()).unwrap();
        shell.run_script("customfn() { :; }");
        shell
            .register_embedder_builtin("host-command", |_shell, _argv| Ok(ExitStatus::SUCCESS))
            .unwrap();
        let context = CompletionContext::from_shell(&shell);
        let source = "echo x; customfn | host-command && path-tool";
        let styled = highlight_line(source, &context);

        assert_eq!(plain_text(&styled), source);
        for command in ["echo", "customfn", "host-command", "path-tool"] {
            assert_eq!(
                style_at(&styled, source, command).0,
                Some(Color::Green),
                "command={command}"
            );
        }
    }

    #[test]
    fn syntax_highlighter_recognizes_executable_command_paths() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("path-tool");
        fs::write(&executable, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let mut shell = Shell::builder()
            .cwd(temp.path().to_path_buf())
            .env_clear()
            .build();
        shell.set_var("PATH", "").unwrap();
        let context = CompletionContext::from_shell(&shell);
        let source = "./path-tool";
        let styled = highlight_line(source, &context);

        assert_eq!(plain_text(&styled), source);
        assert_eq!(style_at(&styled, source, source).0, Some(Color::Green));
    }

    #[test]
    fn syntax_highlighter_colors_boolean_sequential_background_and_redirect_operators() {
        let context = context();
        let source = "echo hi && echo ok || echo no; echo done & cat <input >output";
        let styled = highlight_line(source, &context);

        assert_eq!(plain_text(&styled), source);
        for operator in ["&&", "||", ";", "&", "<", ">"] {
            assert_eq!(
                style_at(&styled, source, operator).0,
                Some(Color::Blue),
                "operator={operator}"
            );
        }
        let background = "echo done &";
        assert_eq!(
            style_at(&highlight_line(background, &context), background, "&").0,
            Some(Color::Blue)
        );

        let comment = "echo ok;# comment after a list operator";
        assert!(style_at(&highlight_line(comment, &context), comment, "# comment").1);
    }

    #[test]
    fn syntax_highlighter_handles_multiline_and_incomplete_input_without_mutating_it() {
        let context = context();
        let multiline = "echo hi\nmissing-command";
        assert_eq!(plain_text(&highlight_line(multiline, &context)), multiline);

        let incomplete = "echo \"unterminated $HOME";
        let styled = highlight_line(incomplete, &context);
        assert_eq!(plain_text(&styled), incomplete);
        assert_eq!(style_at(&styled, incomplete, "echo").0, None);
    }
}

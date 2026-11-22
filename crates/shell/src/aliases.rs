//! Interactive aliases: the alias table, the `alias`/`unalias` builtins and
//! alias expansion of interactive REPL lines.
//!
//! epsh stays alias-free by design. shellux expands aliases only in lines typed
//! at the interactive prompt, before handing them to epsh; `-c`, scripts,
//! piped input and files sourced with `.` are never expanded.
//!
//! Expansion follows POSIX/dash: only an unquoted literal word in command-name
//! position of a simple command is replaced; the replacement is scanned again
//! (except for aliases already being expanded, which stops recursion), and a
//! value ending in a blank makes the next word a candidate too. Words are found
//! with epsh's own lexer, so quoting, `$(...)`, comments and here-document
//! bodies are never expanded.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use epsh::ast::WordPart;
use epsh::error::ExitStatus;
use epsh::eval::Shell;
use epsh::lexer::{Lexer, PendingHereDoc, Token, parts_have_quoting, parts_to_text};
use epsh::shell_bytes::ShellBytes;

/// Alias table shared by the builtins, the REPL, completion and highlighting.
pub(crate) type SharedAliases = Arc<Mutex<Aliases>>;

#[derive(Clone, Debug, Default)]
pub(crate) struct Aliases {
    map: BTreeMap<String, String>,
}

impl Aliases {
    #[cfg(test)]
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.map.get(name).map(String::as_str)
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(String::as_str)
    }

    /// Expand aliases in an interactive REPL line before epsh parses it.
    ///
    /// Lines that contain no alias in command-name position are returned
    /// unchanged, so their syntax errors and diagnostics stay epsh's own.
    pub(crate) fn expand(&self, source: &str) -> String {
        if self.map.is_empty() {
            return source.to_owned();
        }
        self.expand_text(source, &mut Vec::new()).0
    }

    /// Expand `text`; returns the result and whether the next word after it
    /// would be in command-name position. `active` holds the aliases being
    /// expanded, which are not expanded again (recursion guard).
    fn expand_text(&self, text: &str, active: &mut Vec<String>) -> (String, bool) {
        let chars = text.chars().collect::<Vec<_>>();
        let byte_offsets = text
            .char_indices()
            .map(|(offset, _)| offset)
            .chain(std::iter::once(text.len()))
            .collect::<Vec<_>>();
        let mut output = String::with_capacity(text.len());
        let mut copied = 0;
        let mut lexer = Lexer::new(text);
        let mut state = CommandPosition::default();
        loop {
            lexer.recognize_reserved = state.recognizes_reserved_words();
            let Ok((token, span)) = lexer.next_token() else {
                // Leave the rest for epsh to diagnose.
                break;
            };
            let start = span.offset.min(chars.len());
            let end = lexer.span().offset.min(chars.len());
            match token {
                Token::Eof => break,
                Token::Newline => {
                    // Here-document bodies follow the newline; skip them whole.
                    let mut failed = false;
                    for heredoc in std::mem::take(&mut state.pending_heredocs) {
                        if lexer.read_heredoc_body(&heredoc).is_err() {
                            failed = true;
                            break;
                        }
                    }
                    if failed {
                        break;
                    }
                    state.separator(true);
                }
                Token::Word(parts, had_quoting) => {
                    let io_number = !had_quoting
                        && literal(&parts)
                            .is_some_and(|text| text.bytes().all(|byte| byte.is_ascii_digit()))
                        && matches!(chars.get(end), Some('<' | '>'));
                    if io_number {
                        continue;
                    }
                    if let Some(strip_tabs) = state.heredoc_delimiter.take() {
                        state.pending_heredocs.push(PendingHereDoc {
                            delimiter: parts_to_text(&parts).into(),
                            strip_tabs,
                            quoted: had_quoting || parts_have_quoting(&parts),
                        });
                        continue;
                    }
                    if std::mem::take(&mut state.redirection_target) {
                        continue;
                    }
                    let word = if had_quoting { None } else { literal(&parts) };
                    if state.case_patterns {
                        if word.as_deref() == Some("esac") {
                            state.case_patterns = false;
                            state.expect_command = false;
                        }
                        continue;
                    }
                    if std::mem::take(&mut state.case_header) {
                        if word.as_deref() == Some("in") {
                            state.case_patterns = true;
                            continue;
                        }
                        state.case_header = true;
                    }
                    state.after_lparen = false;
                    if !state.expect_command {
                        continue;
                    }
                    state.expect_command = false;
                    let Some(name) = word else { continue };
                    if active.contains(&name) {
                        continue;
                    }
                    let Some(value) = self.map.get(&name) else {
                        continue;
                    };
                    active.push(name);
                    let (replacement, next_is_command) = self.expand_text(value, active);
                    active.pop();
                    output.push_str(&text[byte_offsets[copied]..byte_offsets[start]]);
                    output.push_str(&replacement);
                    copied = end;
                    state.expect_command = next_is_command || value.ends_with([' ', '\t']);
                }
                Token::Assignment { name, value } => {
                    if let Some(strip_tabs) = state.heredoc_delimiter.take() {
                        state.pending_heredocs.push(PendingHereDoc {
                            delimiter: format!("{name}={}", parts_to_text(&value)).into(),
                            strip_tabs,
                            quoted: parts_have_quoting(&value),
                        });
                    } else {
                        // A prefix assignment keeps the command-name position.
                        state.redirection_target = false;
                    }
                }
                token if state.case_patterns => {
                    if token == Token::RParen {
                        state.case_patterns = false;
                        state.expect_command = true;
                    }
                }
                Token::Semi | Token::Amp | Token::Pipe | Token::And | Token::Or => {
                    state.separator(true);
                }
                Token::LParen => {
                    state.separator(true);
                    state.after_lparen = true;
                }
                Token::RParen => {
                    // `name()` is a function definition: its body follows.
                    let function_definition = state.after_lparen;
                    state.separator(function_definition);
                }
                Token::SemiSemi => {
                    state.separator(false);
                    state.case_patterns = true;
                }
                Token::DLess | Token::DLessDash => {
                    state.heredoc_delimiter = Some(token == Token::DLessDash);
                }
                token if token.is_redir() => state.redirection_target = true,
                Token::If
                | Token::Then
                | Token::Else
                | Token::Elif
                | Token::Do
                | Token::While
                | Token::Until
                | Token::Bang
                | Token::Lbrace => state.separator(true),
                Token::Case => {
                    state.separator(false);
                    state.case_header = true;
                }
                _ => state.separator(false),
            }
        }
        output.push_str(&text[byte_offsets[copied]..]);
        (output, state.expect_command)
    }
}

/// Scanner state for finding command-name positions in one input text.
struct CommandPosition {
    expect_command: bool,
    redirection_target: bool,
    /// `Some(strip_tabs)` after `<<`/`<<-`, until the delimiter word.
    heredoc_delimiter: Option<bool>,
    pending_heredocs: Vec<PendingHereDoc>,
    /// After `case`, until the word `in`.
    case_header: bool,
    /// Between `in`/`;;` and the `)` that ends a case pattern list.
    case_patterns: bool,
    after_lparen: bool,
}

impl Default for CommandPosition {
    /// Every input text starts in command-name position.
    fn default() -> Self {
        Self {
            expect_command: true,
            redirection_target: false,
            heredoc_delimiter: None,
            pending_heredocs: Vec::new(),
            case_header: false,
            case_patterns: false,
            after_lparen: false,
        }
    }
}

impl CommandPosition {
    fn recognizes_reserved_words(&self) -> bool {
        self.expect_command
            && !self.redirection_target
            && self.heredoc_delimiter.is_none()
            && !self.case_patterns
    }

    fn separator(&mut self, expect_command: bool) {
        self.expect_command = expect_command;
        self.redirection_target = false;
        self.after_lparen = false;
    }
}

/// The text of a word made of one unquoted literal part.
fn literal(parts: &[WordPart]) -> Option<String> {
    match parts {
        [WordPart::Literal(text)] => Some(text.to_shell_string()),
        _ => None,
    }
}

/// POSIX alias-name characters: letters, digits, `_`, `!`, `%`, `,` and `@`.
pub(crate) fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '!' | '%' | ',' | '@'))
}

/// Quote a value for `alias` output, escaping single quotes as `'\''`.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Lock a shared table, recovering from a poisoned lock.
pub(crate) fn lock(aliases: &SharedAliases) -> MutexGuard<'_, Aliases> {
    aliases
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Register `alias` and `unalias` as shellux embedder builtins sharing one table.
pub(crate) fn register(shell: &mut Shell) -> Result<SharedAliases, String> {
    let aliases = SharedAliases::default();
    let alias_table = aliases.clone();
    shell.register_embedder_builtin("alias", move |shell, argv| {
        Ok(alias_builtin(
            shell,
            &mut lock(&alias_table),
            &operands(argv),
        ))
    })?;
    let unalias_table = aliases.clone();
    shell.register_embedder_builtin("unalias", move |shell, argv| {
        Ok(unalias_builtin(
            shell,
            &mut lock(&unalias_table),
            &operands(argv),
        ))
    })?;
    Ok(aliases)
}

fn operands(argv: &[ShellBytes]) -> Vec<String> {
    argv.iter()
        .skip(1)
        .map(ShellBytes::to_shell_string)
        .collect()
}

fn write_out(shell: &mut Shell, text: &str) {
    shell.write_stdout(ShellBytes::from_str_lossless(text).as_bytes());
}

fn write_err(shell: &mut Shell, text: &str) {
    shell.write_stderr(ShellBytes::from_str_lossless(text).as_bytes());
}

/// `alias [name[=value] ...]`
fn alias_builtin(shell: &mut Shell, table: &mut Aliases, args: &[String]) -> ExitStatus {
    let args = match args.first().map(String::as_str) {
        Some("--") => &args[1..],
        _ => args,
    };
    if args.is_empty() {
        let listing = table
            .map
            .iter()
            .map(|(name, value)| format!("alias {name}={}\n", quote(value)))
            .collect::<String>();
        write_out(shell, &listing);
        return ExitStatus::SUCCESS;
    }
    let mut status = ExitStatus::SUCCESS;
    for arg in args {
        if let Some((name, value)) = arg.split_once('=') {
            if is_valid_name(name) {
                table.map.insert(name.to_owned(), value.to_owned());
            } else {
                write_err(shell, &format!("alias: invalid alias name: {name}\n"));
                status = ExitStatus::FAILURE;
            }
        } else if let Some(value) = table.map.get(arg) {
            let line = format!("alias {arg}={}\n", quote(value));
            write_out(shell, &line);
        } else {
            write_err(shell, &format!("alias: {arg}: not found\n"));
            status = ExitStatus::FAILURE;
        }
    }
    status
}

/// `unalias -a` or `unalias name...`
fn unalias_builtin(shell: &mut Shell, table: &mut Aliases, args: &[String]) -> ExitStatus {
    let args = match args.first().map(String::as_str) {
        Some("-a") => {
            table.map.clear();
            return ExitStatus::SUCCESS;
        }
        Some("--") => &args[1..],
        _ => args,
    };
    if args.is_empty() {
        write_err(shell, "unalias: usage: unalias [-a] name [name ...]\n");
        return ExitStatus::MISUSE;
    }
    let mut status = ExitStatus::SUCCESS;
    for name in args {
        if table.map.remove(name).is_none() {
            write_err(shell, &format!("unalias: {name}: not found\n"));
            status = ExitStatus::FAILURE;
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, &str)]) -> Aliases {
        Aliases {
            map: pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }

    fn ll() -> Aliases {
        table(&[("ll", "ls -l")])
    }

    #[test]
    fn expands_at_start_of_line() {
        assert_eq!(ll().expand("ll"), "ls -l");
        assert_eq!(ll().expand("  ll /tmp"), "  ls -l /tmp");
    }

    #[test]
    fn expands_after_list_and_pipeline_operators() {
        let aliases = ll();
        assert_eq!(aliases.expand("true; ll"), "true; ls -l");
        assert_eq!(aliases.expand("echo x | ll"), "echo x | ls -l");
        assert_eq!(aliases.expand("true && ll"), "true && ls -l");
        assert_eq!(aliases.expand("false || ll"), "false || ls -l");
        assert_eq!(aliases.expand("sleep 1 & ll"), "sleep 1 & ls -l");
        assert_eq!(aliases.expand("(ll)"), "(ls -l)");
        assert_eq!(aliases.expand("true\nll"), "true\nls -l");
    }

    #[test]
    fn expands_after_reserved_words_that_start_a_command() {
        let aliases = ll();
        assert_eq!(
            aliases.expand("if ll; then ll; elif ll; then ll; else ll; fi"),
            "if ls -l; then ls -l; elif ls -l; then ls -l; else ls -l; fi"
        );
        assert_eq!(
            aliases.expand("while ll; do ll; done"),
            "while ls -l; do ls -l; done"
        );
        assert_eq!(
            aliases.expand("until ll; do :; done"),
            "until ls -l; do :; done"
        );
        assert_eq!(aliases.expand("! ll"), "! ls -l");
        assert_eq!(aliases.expand("{ ll; }"), "{ ls -l; }");
        assert_eq!(
            aliases.expand("for x in ll; do ll; done"),
            "for x in ll; do ls -l; done"
        );
    }

    #[test]
    fn expands_after_assignments_and_redirections() {
        let aliases = ll();
        assert_eq!(aliases.expand("A=1 ll"), "A=1 ls -l");
        assert_eq!(aliases.expand(">out ll"), ">out ls -l");
        assert_eq!(aliases.expand("2>/dev/null ll"), "2>/dev/null ls -l");
    }

    #[test]
    fn does_not_expand_in_argument_position() {
        let aliases = ll();
        assert_eq!(aliases.expand("echo ll"), "echo ll");
        assert_eq!(aliases.expand("echo then ll"), "echo then ll");
        assert_eq!(aliases.expand("echo ll; echo ll"), "echo ll; echo ll");
        assert_eq!(aliases.expand("cat <ll"), "cat <ll");
    }

    #[test]
    fn does_not_expand_quoted_or_escaped_words() {
        let aliases = ll();
        for line in ["'ll'", "\\ll", "\"ll\"", "l'l'", "$ll", "ll$x"] {
            assert_eq!(aliases.expand(line), line);
        }
    }

    #[test]
    fn does_not_expand_inside_command_substitution_comment_or_heredoc() {
        let aliases = ll();
        assert_eq!(aliases.expand("echo $(ll)"), "echo $(ll)");
        assert_eq!(aliases.expand("echo `ll`"), "echo `ll`");
        assert_eq!(aliases.expand("echo x # ll"), "echo x # ll");
        assert_eq!(aliases.expand("# ll"), "# ll");
        assert_eq!(
            aliases.expand("cat <<EOF\nll\nEOF\nll"),
            "cat <<EOF\nll\nEOF\nls -l"
        );
        assert_eq!(
            aliases.expand("cat <<-'E'\n\tll\n\tE\nll"),
            "cat <<-'E'\n\tll\n\tE\nls -l"
        );
    }

    #[test]
    fn does_not_expand_case_patterns() {
        let aliases = ll();
        assert_eq!(
            aliases.expand("case x in ll) ll;; (ll|x) ll;; esac"),
            "case x in ll) ls -l;; (ll|x) ls -l;; esac"
        );
    }

    #[test]
    fn recursion_guard_stops_self_and_mutual_recursion() {
        assert_eq!(table(&[("ls", "ls -F")]).expand("ls /"), "ls -F /");
        let mutual = table(&[("a", "b x"), ("b", "a y")]);
        assert_eq!(mutual.expand("a"), "a y x");
        assert_eq!(mutual.expand("b"), "b x y");
        let nested = table(&[("ll", "ls -l"), ("ls", "ls -F")]);
        assert_eq!(nested.expand("ll"), "ls -F -l");
    }

    #[test]
    fn alias_value_operators_start_new_commands() {
        let aliases = table(&[("x", "cd /;"), ("ll", "ls -l")]);
        assert_eq!(aliases.expand("x ll"), "cd /; ls -l");
        assert_eq!(table(&[("x", "echo")]).expand("x x"), "echo x");
    }

    #[test]
    fn trailing_blank_checks_the_next_word() {
        let aliases = table(&[("sudo", "sudo "), ("ll", "ls -l")]);
        assert_eq!(aliases.expand("sudo ll"), "sudo  ls -l");
        assert_eq!(aliases.expand("sudo apt ll"), "sudo  apt ll");
        let no_blank = table(&[("sudo", "sudo"), ("ll", "ls -l")]);
        assert_eq!(no_blank.expand("sudo ll"), "sudo ll");
    }

    #[test]
    fn lines_without_aliases_are_unchanged() {
        let aliases = ll();
        for line in ["echo 'unterminated", "fi", "if true; then", "echo a |", ""] {
            assert_eq!(aliases.expand(line), line);
        }
        assert_eq!(Aliases::default().expand("ll"), "ll");
    }

    #[test]
    fn name_validity_follows_posix_alias_characters() {
        for name in ["ll", "A_1", "!", "%x", "a,b", "@h"] {
            assert!(is_valid_name(name), "{name}");
        }
        for name in ["", "a.b", "a-b", "a/b", "a b", "$x", "é"] {
            assert!(!is_valid_name(name), "{name}");
        }
    }

    struct Run {
        status: i32,
        stdout: String,
        stderr: String,
    }

    fn shell_with_aliases() -> (Shell, SharedAliases, SharedBuf, SharedBuf) {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .env_clear()
            .stdout_sink(stdout.clone())
            .stderr_sink(stderr.clone())
            .build();
        let aliases = register(&mut shell).unwrap();
        (shell, aliases, stdout, stderr)
    }

    type SharedBuf = Arc<Mutex<Vec<u8>>>;

    fn run(shell: &mut Shell, stdout: &SharedBuf, stderr: &SharedBuf, script: &str) -> Run {
        stdout.lock().unwrap().clear();
        stderr.lock().unwrap().clear();
        let status = shell.run_script(script);
        Run {
            status,
            stdout: String::from_utf8(stdout.lock().unwrap().clone()).unwrap(),
            stderr: String::from_utf8(stderr.lock().unwrap().clone()).unwrap(),
        }
    }

    #[test]
    fn alias_defines_and_lists_sorted_with_quoting() {
        let (mut shell, aliases, out, err) = shell_with_aliases();
        let r = run(&mut shell, &out, &err, "alias ll='ls -l' a=\"it's\" e=");
        assert_eq!(
            (r.status, r.stdout.as_str(), r.stderr.as_str()),
            (0, "", "")
        );
        assert_eq!(lock(&aliases).get("ll"), Some("ls -l"));
        let r = run(&mut shell, &out, &err, "alias");
        assert_eq!(r.status, 0);
        assert_eq!(
            r.stdout,
            "alias a='it'\\''s'\nalias e=''\nalias ll='ls -l'\n"
        );
        assert_eq!(r.stderr, "");
        let r = run(&mut shell, &out, &err, "alias ll");
        assert_eq!((r.status, r.stdout.as_str()), (0, "alias ll='ls -l'\n"));
    }

    #[test]
    fn alias_value_may_contain_equals_and_redefinition_replaces() {
        let (mut shell, aliases, out, err) = shell_with_aliases();
        let r = run(&mut shell, &out, &err, "alias x=a=b; alias x=c");
        assert_eq!(r.status, 0);
        assert_eq!(lock(&aliases).get("x"), Some("c"));
        run(&mut shell, &out, &err, "alias y=a=b");
        assert_eq!(lock(&aliases).get("y"), Some("a=b"));
    }

    #[test]
    fn alias_unknown_name_reports_and_keeps_processing() {
        let (mut shell, aliases, out, err) = shell_with_aliases();
        let r = run(&mut shell, &out, &err, "alias nope k=v");
        assert_eq!(r.status, 1);
        assert_eq!(r.stdout, "");
        assert_eq!(r.stderr, "alias: nope: not found\n");
        assert_eq!(lock(&aliases).get("k"), Some("v"));
    }

    #[test]
    fn alias_invalid_name_is_rejected() {
        let (mut shell, aliases, out, err) = shell_with_aliases();
        let r = run(&mut shell, &out, &err, "alias a.b=x ok=y");
        assert_eq!(r.status, 1);
        assert_eq!(r.stdout, "");
        assert_eq!(r.stderr, "alias: invalid alias name: a.b\n");
        assert_eq!(lock(&aliases).get("a.b"), None);
        assert_eq!(lock(&aliases).get("ok"), Some("y"));
    }

    #[test]
    fn unalias_removes_names_and_all() {
        let (mut shell, aliases, out, err) = shell_with_aliases();
        run(&mut shell, &out, &err, "alias a=1 b=2 c=3");
        let r = run(&mut shell, &out, &err, "unalias a");
        assert_eq!(
            (r.status, r.stdout.as_str(), r.stderr.as_str()),
            (0, "", "")
        );
        assert_eq!(lock(&aliases).names().collect::<Vec<_>>(), ["b", "c"]);
        let r = run(&mut shell, &out, &err, "unalias -a");
        assert_eq!(
            (r.status, r.stdout.as_str(), r.stderr.as_str()),
            (0, "", "")
        );
        assert_eq!(lock(&aliases).names().count(), 0);
    }

    #[test]
    fn unalias_unknown_name_reports_and_keeps_processing() {
        let (mut shell, aliases, out, err) = shell_with_aliases();
        run(&mut shell, &out, &err, "alias b=2");
        let r = run(&mut shell, &out, &err, "unalias nope b");
        assert_eq!(r.status, 1);
        assert_eq!(r.stdout, "");
        assert_eq!(r.stderr, "unalias: nope: not found\n");
        assert_eq!(lock(&aliases).names().count(), 0);
    }

    #[test]
    fn unalias_without_operands_prints_usage() {
        let (mut shell, _aliases, out, err) = shell_with_aliases();
        let r = run(&mut shell, &out, &err, "unalias");
        assert_eq!(r.status, 2);
        assert_eq!(r.stderr, "unalias: usage: unalias [-a] name [name ...]\n");
    }

    #[test]
    fn builtins_are_reported_by_command_v() {
        let (mut shell, _aliases, out, err) = shell_with_aliases();
        let r = run(&mut shell, &out, &err, "command -v alias unalias");
        assert_eq!((r.status, r.stdout.as_str()), (0, "alias\nunalias\n"));
    }
}

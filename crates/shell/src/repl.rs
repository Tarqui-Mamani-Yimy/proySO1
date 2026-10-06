use std::borrow::Cow;
use std::io;
use std::ops::ControlFlow;
use std::path::PathBuf;

use epsh::eval::Shell;
use epsh::parser::Parser;
use reedline::{
    DefaultPrompt, FileBackedHistory, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    Signal, ValidationResult, Validator,
};

struct ShellValidator;

impl Validator for ShellValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        // Enter terminates the final physical line, which may be an empty
        // heredoc delimiter. Reedline validates before inserting that newline.
        let submitted = format!("{line}\n");
        if matches!(Parser::new(&submitted).parse_interactive(), Ok(None)) {
            ValidationResult::Incomplete
        } else {
            // Invalid input is submitted to epsh for its normal diagnostic/status.
            ValidationResult::Complete
        }
    }
}

struct ShellPrompt {
    primary: String,
    continuation: String,
}

impl ShellPrompt {
    fn from_shell(shell: &Shell) -> Self {
        fn value(shell: &Shell, name: &str, default: &str) -> String {
            shell.get_var_bytes(name).map_or_else(
                || default.to_owned(),
                // reedline's Prompt requires str: preserve valid Unicode, replace
                // invalid UTF-8 for display only, without changing the shell value.
                |bytes| String::from_utf8_lossy(bytes.as_bytes()).into_owned(),
            )
        }
        Self {
            primary: value(shell, "PS1", "$ "),
            continuation: value(shell, "PS2", "> "),
        }
    }
}

impl Prompt for ShellPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.primary)
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.continuation)
    }
    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        DefaultPrompt::default()
            .render_prompt_history_search_indicator(search)
            .into_owned()
            .into()
    }
}

fn history_path(shell: &Shell) -> Option<PathBuf> {
    shell
        .get_var_bytes("SO1SH_HISTFILE")
        .filter(|v| !v.is_empty())
        .map(|v| PathBuf::from(v.to_os_string()))
        .or_else(|| {
            shell
                .get_var_bytes("HOME")
                .filter(|v| !v.is_empty())
                .map(|v| PathBuf::from(v.to_os_string()).join(".so1sh_history"))
        })
}

/// Reedline owns editing/history; epsh owns parsing, evaluation, and exit semantics.
pub fn run(shell: &mut Shell) -> i32 {
    match read_lines(shell) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("{}: {error}", env!("CARGO_PKG_NAME"));
            1
        }
    }
}

fn read_lines(shell: &mut Shell) -> io::Result<i32> {
    let terminal = crate::terminal::Terminal::new()?;
    shell.opts_mut().interactive = true;
    let mut editor = Reedline::create().with_validator(Box::new(ShellValidator));
    if let Some(path) = history_path(shell) {
        match FileBackedHistory::with_file(reedline::HISTORY_SIZE, path) {
            Ok(history) => editor = editor.with_history(Box::new(history)),
            Err(error) => eprintln!("so1sh: history: {error}"),
        }
    }
    loop {
        match editor.read_line(&ShellPrompt::from_shell(shell))? {
            Signal::Success(source) => {
                // Flush before evaluation, including exit, so history survives sessions.
                if let Err(error) = editor.sync_history() {
                    eprintln!("so1sh: history: {error}");
                }
                terminal.restore()?; // Never launch a child with editor raw modes.
                let result = shell.eval_repl_line(&source);
                shell.discard_stopped_jobs();
                terminal.restore()?;
                if let ControlFlow::Break(status) = result {
                    return Ok(status.code());
                }
            }
            Signal::CtrlD => return Ok(shell.finish_repl().code()),
            Signal::CtrlC => {} // reedline discards the buffer; keep the shell alive.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epsh::shell_bytes::ShellBytes;

    #[test]
    fn unicode_prompt_replacement_does_not_change_engine_bytes() {
        let mut shell = Shell::builder().env_clear().build();
        shell
            .set_var_bytes("PS1", ShellBytes::from_vec(b"prompt\xff> ".to_vec()))
            .unwrap();
        shell.set_var("PS2", "continued> ").unwrap();
        let prompt = ShellPrompt::from_shell(&shell);
        assert_eq!(prompt.primary, "prompt\u{fffd}> ");
        assert_eq!(prompt.continuation, "continued> ");
        assert_eq!(
            shell.get_var_bytes("PS1").unwrap().as_bytes(),
            b"prompt\xff> "
        );
    }

    #[test]
    fn history_path_precedence_and_bytes() {
        let mut shell = Shell::builder().env_clear().build();
        assert_eq!(history_path(&shell), None);
        shell.set_var("HOME", "/private/home").unwrap();
        assert_eq!(
            history_path(&shell),
            Some(PathBuf::from("/private/home/.so1sh_history"))
        );
        shell
            .set_var_bytes(
                "SO1SH_HISTFILE",
                ShellBytes::from_vec(b"history\xff".to_vec()),
            )
            .unwrap();
        assert_eq!(
            history_path(&shell),
            Some(ShellBytes::from_vec(b"history\xff".to_vec()).to_path_buf())
        );
    }

    #[test]
    fn empty_history_variables_are_unset() {
        let mut shell = Shell::builder().env_clear().build();
        shell.set_var("HOME", "").unwrap();
        assert_eq!(history_path(&shell), None);
        shell.set_var("SO1SH_HISTFILE", "").unwrap();
        assert_eq!(history_path(&shell), None);
        shell.set_var("HOME", "/private/home").unwrap();
        assert_eq!(
            history_path(&shell),
            Some(PathBuf::from("/private/home/.so1sh_history"))
        );
        shell.set_var("SO1SH_HISTFILE", "/private/history").unwrap();
        shell.set_var("HOME", "").unwrap();
        assert_eq!(
            history_path(&shell),
            Some(PathBuf::from("/private/history"))
        );
    }

    #[test]
    fn validator_enter_terminates_empty_heredoc_delimiter() {
        let validator = ShellValidator;
        for input in ["cat <<''", "cat <<''\nbody"] {
            assert!(
                matches!(validator.validate(input), ValidationResult::Incomplete),
                "{input:?}"
            );
        }
        for input in ["cat <<''\n", "cat <<''\nbody\n", "cat <<-''\n\t"] {
            assert!(
                matches!(validator.validate(input), ValidationResult::Complete),
                "{input:?}"
            );
        }
    }

    #[test]
    fn validator_submits_invalid_input_instead_of_waiting() {
        let validator = ShellValidator;
        assert!(matches!(
            validator.validate("fi"),
            ValidationResult::Complete
        ));
        assert!(matches!(
            validator.validate("if true; then"),
            ValidationResult::Incomplete
        ));
        assert!(matches!(
            validator.validate("cat <<EOF\nbody\nEOF"),
            ValidationResult::Complete
        ));
    }
}

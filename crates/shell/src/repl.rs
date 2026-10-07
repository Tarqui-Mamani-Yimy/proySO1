use std::borrow::Cow;
use std::io;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use epsh::eval::Shell;
use epsh::parser::Parser;
use reedline::{
    DefaultPrompt, FileBackedHistory, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    SemanticPromptMarkers, Signal, ValidationResult, Validator,
};

const OSC133_PROMPT_START: &[u8] = b"\x1b]133;A\x1b\\";
const OSC133_COMMAND_EXECUTED: &[u8] = b"\x1b]133;C\x1b\\";

#[derive(Default)]
struct PromptMarkerState {
    command_input_started: AtomicBool,
    prompt_kind: AtomicU8,
}

impl PromptMarkerState {
    fn begin_line(&self) {
        self.command_input_started.store(false, Ordering::Relaxed);
        self.prompt_kind.store(0, Ordering::Relaxed);
    }
}

struct ShellSemanticMarkers(Arc<PromptMarkerState>);

impl SemanticPromptMarkers for ShellSemanticMarkers {
    fn prompt_start(&self, kind: reedline::PromptKind) -> Cow<'_, str> {
        // OSC 133;A is sent to /dev/tty immediately before Reedline paints.
        // Continuation prompts remain inside that same input block.
        let kind = match kind {
            reedline::PromptKind::Primary => 1,
            reedline::PromptKind::Secondary => 2,
            reedline::PromptKind::Right => 3,
        };
        self.0.prompt_kind.store(kind, Ordering::Relaxed);
        Cow::Borrowed("")
    }

    fn command_input_start(&self) -> Cow<'_, str> {
        if self.0.prompt_kind.load(Ordering::Relaxed) != 1 {
            return Cow::Borrowed("");
        }
        let was_started = self.0.command_input_started.swap(true, Ordering::Relaxed);
        if was_started {
            Cow::Borrowed("")
        } else {
            Cow::Borrowed("\x1b]133;B\x1b\\")
        }
    }
}

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

fn startup_rc_command(shell: &Shell) -> Option<&'static str> {
    if shell
        .get_var_bytes("SO1SH_RC")
        .is_some_and(|v| !v.is_empty())
    {
        return Some(". \"$SO1SH_RC\"");
    }
    let home = shell.get_var_bytes("HOME").filter(|v| !v.is_empty())?;
    let path = home.to_path_buf().join(".so1shrc");
    // Only a missing default is silent; let '.' diagnose other access failures.
    (!matches!(path.try_exists(), Ok(false))).then_some(". \"$HOME/.so1shrc\"")
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
    let jobs = crate::jobs::register(shell)
        .map_err(|error| io::Error::other(format!("so1sh: jobs builtin: {error}")))?;
    shell.suppress_repl_stopped_diagnostic(true);
    let terminal = match crate::terminal::Terminal::acquire() {
        Ok(terminal) => Some(terminal),
        Err(error) => {
            eprintln!("so1sh: terminal: {error}; continuing without terminal job control");
            crate::terminal::ignore_prompt_signals()?;
            None
        }
    };
    shell.opts_mut().interactive = true;
    let shell_integration_enabled = std::env::var("SO1SH_SHELL_INTEGRATION").as_deref() != Ok("0")
        && terminal
            .as_ref()
            .is_some_and(crate::terminal::Terminal::stdout_is_controlling_tty);
    let marker_state = Arc::new(PromptMarkerState::default());
    if let Some(command) = startup_rc_command(shell) {
        // Quoted variable expansion preserves path bytes without interpreting
        // the filename as shell syntax. Reuse '.' and normal REPL error/exit flow.
        let result = shell.eval_repl_line(command);
        crate::jobs::sync(shell, &jobs);
        if let Some(terminal) = &terminal {
            terminal.reclaim()?;
        }
        if let ControlFlow::Break(status) = result {
            return Ok(status.code());
        }
    }
    let mut editor = Reedline::create().with_validator(Box::new(ShellValidator));
    if shell_integration_enabled {
        editor = editor.with_semantic_markers(Some(Box::new(ShellSemanticMarkers(Arc::clone(
            &marker_state,
        )))));
    }
    if let Some(path) = history_path(shell) {
        match FileBackedHistory::with_file(reedline::HISTORY_SIZE, path) {
            Ok(history) => editor = editor.with_history(Box::new(history)),
            Err(error) => eprintln!("so1sh: history: {error}"),
        }
    }
    loop {
        if shell_integration_enabled {
            let terminal = terminal
                .as_ref()
                .expect("enabled integration has a terminal");
            terminal.report_cwd(shell.cwd())?;
            terminal.write_shell_integration(OSC133_PROMPT_START)?;
            marker_state.begin_line();
        }
        match editor.read_line(&ShellPrompt::from_shell(shell))? {
            Signal::Success(source) => {
                // Flush before evaluation, including exit, so history survives sessions.
                if let Err(error) = editor.sync_history() {
                    eprintln!("so1sh: history: {error}");
                }
                if let Some(terminal) = &terminal {
                    terminal.reclaim()?;
                }
                if shell_integration_enabled {
                    terminal
                        .as_ref()
                        .expect("enabled integration has a terminal")
                        .write_shell_integration(OSC133_COMMAND_EXECUTED)?;
                }
                let result = shell.eval_repl_line(&source);
                crate::jobs::sync(shell, &jobs);
                if let Some(terminal) = &terminal {
                    terminal.reclaim()?;
                }
                if shell_integration_enabled {
                    let status = match &result {
                        ControlFlow::Continue(status) | ControlFlow::Break(status) => status.code(),
                    };
                    let finished = format!("\x1b]133;D;{status}\x1b\\");
                    terminal
                        .as_ref()
                        .expect("enabled integration has a terminal")
                        .write_shell_integration(finished.as_bytes())?;
                }
                if let ControlFlow::Break(status) = result {
                    return Ok(status.code());
                }
            }
            Signal::CtrlD => {
                if let Some(status) = shell.try_finish_repl() {
                    return Ok(status.code());
                }
            }
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

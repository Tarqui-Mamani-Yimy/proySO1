use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use epsh::builtins::BUILTIN_NAMES;
use epsh::eval::Shell;
use epsh::lexer::{Lexer, Token};
use reedline::{Completer, CompletionResult, Span, Suggestion};

#[derive(Clone, Debug, Default)]
pub(crate) struct CompletionContext {
    cwd: PathBuf,
    command_names: BTreeSet<String>,
    variables: Vec<String>,
}

impl CompletionContext {
    pub(crate) fn from_shell(shell: &Shell) -> Self {
        let cwd = shell.cwd().to_path_buf();
        let path = shell
            .get_var_bytes("PATH")
            .map(|value| {
                std::env::split_paths(&value.to_os_string())
                    .map(|directory| {
                        if directory.is_absolute() {
                            directory
                        } else {
                            cwd.join(directory)
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut command_names = BUILTIN_NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<BTreeSet<_>>();
        command_names.extend(shell.embedder_builtin_names().map(str::to_owned));
        command_names.extend(shell.functions().keys().cloned());
        command_names.extend(executable_names(&path));

        Self {
            cwd,
            command_names,
            variables: shell.vars().names().map(str::to_owned).collect(),
        }
    }

    pub(crate) fn resolves_command(&self, name: &str) -> bool {
        if self.command_names.contains(name) {
            return true;
        }
        if !name.contains('/') {
            return false;
        }
        let path = Path::new(name);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        is_executable_file(&path)
    }
}

#[derive(Clone)]
pub(crate) struct CompletionState(Arc<RwLock<CompletionContext>>);

impl CompletionState {
    pub(crate) fn new(shell: &Shell) -> Self {
        Self(Arc::new(RwLock::new(CompletionContext::from_shell(shell))))
    }

    pub(crate) fn refresh(&self, shell: &Shell) {
        *self
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            CompletionContext::from_shell(shell);
    }

    pub(crate) fn with_context<T>(&self, use_context: impl FnOnce(&CompletionContext) -> T) -> T {
        let context = self
            .0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        use_context(&context)
    }
}

pub(crate) struct ShellCompleter {
    state: CompletionState,
}

impl ShellCompleter {
    pub(crate) fn new(state: CompletionState) -> Self {
        Self { state }
    }
}

impl Completer for ShellCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let context = self
            .state
            .0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        CompletionResult::fresh(complete(line, pos, &context))
    }
}

fn complete(line: &str, pos: usize, context: &CompletionContext) -> Vec<Suggestion> {
    let pos = pos.min(line.len());
    if !line.is_char_boundary(pos) {
        return Vec::new();
    }
    let start = current_word_start(&line[..pos]);
    let raw_prefix = &line[start..pos];

    if let Some((variable_start, prefix, braced)) = variable_prefix(raw_prefix) {
        return context
            .variables
            .iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| {
                let value = if braced && !line[pos..].starts_with('}') {
                    format!("{name}}}")
                } else {
                    name.clone()
                };
                Suggestion {
                    value,
                    span: Span::new(start + variable_start, pos),
                    append_whitespace: false,
                    ..Suggestion::default()
                }
            })
            .collect();
    }

    let decoded_prefix = unescape_shell_word(raw_prefix);
    if command_position(&line[..start]) && decoded_prefix.contains('/') {
        file_completions(&decoded_prefix, start, pos, context, true)
    } else if command_position(&line[..start]) {
        command_completions(&decoded_prefix, start, pos, context)
    } else {
        file_completions(&decoded_prefix, start, pos, context, false)
    }
}

fn command_completions(
    prefix: &str,
    start: usize,
    pos: usize,
    context: &CompletionContext,
) -> Vec<Suggestion> {
    context
        .command_names
        .iter()
        .filter(|name| name.starts_with(prefix))
        .map(|name| Suggestion {
            value: escape_shell_word(name),
            span: Span::new(start, pos),
            append_whitespace: true,
            ..Suggestion::default()
        })
        .collect()
}

fn executable_names(path: &[PathBuf]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for directory in path {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if !is_executable_file(&entry.path()) {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                names.insert(name.to_owned());
            }
        }
    }
    names
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn file_completions(
    prefix: &str,
    start: usize,
    pos: usize,
    context: &CompletionContext,
    executable_commands_only: bool,
) -> Vec<Suggestion> {
    let (directory_prefix, basename_prefix) = prefix
        .rfind('/')
        .map(|slash| (&prefix[..=slash], &prefix[slash + 1..]))
        .unwrap_or(("", prefix));
    let directory = if directory_prefix.is_empty() {
        context.cwd.clone()
    } else {
        let path = Path::new(directory_prefix);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            context.cwd.join(path)
        }
    };
    let Ok(entries) = fs::read_dir(&directory) else {
        return Vec::new();
    };
    let show_hidden = basename_prefix.starts_with('.');
    let mut suggestions = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if (!show_hidden && name.starts_with('.')) || !name.starts_with(basename_prefix) {
            continue;
        }
        let path = directory.join(name);
        let Ok(metadata) = fs::metadata(path) else {
            continue;
        };
        let is_directory = metadata.is_dir();
        let is_executable = metadata.is_file() && metadata.permissions().mode() & 0o111 != 0;
        if executable_commands_only && !is_directory && !is_executable {
            continue;
        }
        let value = format!(
            "{directory_prefix}{name}{}",
            if is_directory { "/" } else { "" }
        );
        suggestions.push(Suggestion {
            value: escape_shell_word(&value),
            span: Span::new(start, pos),
            append_whitespace: !is_directory,
            ..Suggestion::default()
        });
    }
    suggestions.sort_by(|left, right| left.value.cmp(&right.value));
    suggestions
}

pub(crate) fn command_position(prefix: &str) -> bool {
    let mut lexer = Lexer::new(prefix);
    let mut command_seen = false;
    let mut redirection_target = false;
    while let Ok((token, _)) = lexer.next_token() {
        match token {
            Token::Eof => break,
            Token::Pipe | Token::And | Token::Or | Token::Semi | Token::Amp | Token::Newline => {
                command_seen = false;
                redirection_target = false;
            }
            Token::Less
            | Token::Great
            | Token::DGreat
            | Token::DLess
            | Token::DLessDash
            | Token::LessAnd
            | Token::GreatAnd
            | Token::LessGreat
            | Token::Clobber => redirection_target = true,
            Token::Assignment { .. } => {}
            Token::Word(_, _) if redirection_target => redirection_target = false,
            Token::If
            | Token::Then
            | Token::Do
            | Token::Else
            | Token::Elif
            | Token::While
            | Token::Until
            | Token::RParen => {
                command_seen = false;
                redirection_target = false;
            }
            _ => {
                command_seen = true;
                redirection_target = false;
            }
        }
    }
    !command_seen
}

fn current_word_start(prefix: &str) -> usize {
    let mut quote = None;
    let mut escaped = false;
    let mut start = 0;
    for (index, ch) in prefix.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else if ch == '\\' && active_quote == '"' {
                escaped = true;
            }
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '\'' | '"' => quote = Some(ch),
            ch if ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '<' | '>' | '(' | ')') => {
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    start
}

fn variable_prefix(raw: &str) -> Option<(usize, &str, bool)> {
    let mut quote = None;
    let mut escaped = false;
    let mut marker = None;
    for (index, ch) in raw.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else if ch == '\\' && active_quote == '"' {
                escaped = true;
            } else if ch == '$' && active_quote == '"' {
                marker = Some(index);
            }
            if active_quote == '\'' {
                continue;
            }
        } else {
            match ch {
                '\\' => {
                    escaped = true;
                    continue;
                }
                '\'' | '"' => {
                    quote = Some(ch);
                    continue;
                }
                '$' => marker = Some(index),
                _ => {}
            }
        }
    }
    let dollar = marker?;
    let after_dollar = dollar + 1;
    let braced = raw[after_dollar..].starts_with('{');
    let name_start = after_dollar + usize::from(braced);
    let prefix = &raw[name_start..];
    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some((name_start, prefix, braced))
}

pub(crate) fn unescape_shell_word(raw: &str) -> String {
    let mut out = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else if ch == '\\' && active_quote == '"' {
                escaped = true;
            } else {
                out.push(ch);
            }
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '\'' | '"' => quote = Some(ch),
            _ => out.push(ch),
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

fn escape_shell_word(word: &str) -> String {
    let mut escaped = String::with_capacity(word.len());
    for ch in word.chars() {
        if ch.is_whitespace() || "\\'\"$`;&|<>*?()[]{}!#".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::{CompletionContext, complete, executable_names};
    use epsh::error::ExitStatus;
    use epsh::eval::Shell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    fn context(cwd: &Path, path: Vec<PathBuf>) -> CompletionContext {
        CompletionContext {
            cwd: cwd.to_path_buf(),
            command_names: ["echo", "jobs", "greet"]
                .into_iter()
                .map(str::to_owned)
                .chain(executable_names(&path))
                .collect(),
            variables: vec!["PATH".into(), "PROJECT_ROOT".into()],
        }
    }

    fn values(completions: Vec<reedline::Suggestion>) -> Vec<String> {
        completions.into_iter().map(|item| item.value).collect()
    }

    fn executable(path: &Path) {
        fs::write(path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn command_completion_combines_builtins_embedders_functions_and_path() {
        let temp = tempfile::tempdir().unwrap();
        executable(&temp.path().join("unique-tool"));
        let context = context(temp.path(), vec![temp.path().to_path_buf()]);

        assert_eq!(values(complete("uni", 3, &context)), vec!["unique-tool"]);
        assert_eq!(values(complete("ec", 2, &context)), vec!["echo"]);
        assert_eq!(values(complete("jo", 2, &context)), vec!["jobs"]);
        assert_eq!(values(complete("gre", 3, &context)), vec!["greet"]);
    }

    #[test]
    fn command_completion_recognizes_shell_command_separators() {
        let temp = tempfile::tempdir().unwrap();
        executable(&temp.path().join("unique-tool"));
        let context = context(temp.path(), vec![temp.path().to_path_buf()]);

        for line in [
            "uni",
            "echo x | uni",
            "false && uni",
            "false || uni",
            "echo x; uni",
            "echo x\nuni",
        ] {
            let result = complete(line, line.len(), &context);
            assert_eq!(values(result), vec!["unique-tool"], "line={line:?}");
        }
    }

    #[test]
    fn command_path_completion_offers_only_directories_and_executables() {
        let temp = tempfile::tempdir().unwrap();
        executable(&temp.path().join("tool-executable"));
        fs::write(temp.path().join("tool-data"), "").unwrap();
        fs::create_dir(temp.path().join("tool-directory")).unwrap();
        let context = context(temp.path(), vec![]);

        assert_eq!(
            values(complete("./tool-", "./tool-".len(), &context)),
            vec!["./tool-directory/", "./tool-executable"]
        );
    }

    #[test]
    fn path_commands_follow_executable_symlinks_and_skip_broken_links() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        executable(&bin.join("real-tool"));
        symlink("real-tool", bin.join("linked-tool")).unwrap();
        symlink("missing-target", bin.join("broken-tool")).unwrap();
        let mut shell = Shell::builder()
            .cwd(temp.path().to_path_buf())
            .env_clear()
            .build();
        shell.set_var("PATH", bin.to_str().unwrap()).unwrap();
        let context = CompletionContext::from_shell(&shell);

        assert!(context.resolves_command("linked-tool"));
        assert!(!context.resolves_command("broken-tool"));
        assert_eq!(values(complete("linked", 6, &context)), vec!["linked-tool"]);
    }

    #[test]
    fn file_completion_uses_shell_cwd_and_escapes_spaces() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a file.txt"), "").unwrap();
        fs::create_dir(temp.path().join("a dir")).unwrap();
        let context = context(temp.path(), vec![]);

        assert_eq!(
            values(complete("cat a", 5, &context)),
            vec!["a\\ dir/", "a\\ file.txt"]
        );
    }

    #[test]
    fn escaped_special_filename_completion_reparses_to_the_same_path() {
        let temp = tempfile::tempdir().unwrap();
        let filename = "a ;$'[]#file";
        fs::write(temp.path().join(filename), "").unwrap();
        let context = context(temp.path(), vec![]);
        let prefix = format!("test -e {}/a", temp.path().display());
        let matches = complete(&prefix, prefix.len(), &context);
        assert_eq!(matches.len(), 1);

        let mut shell = Shell::builder()
            .cwd(temp.path().to_path_buf())
            .env_clear()
            .stdout_sink(Arc::new(Mutex::new(Vec::<u8>::new())))
            .build();
        let script = format!("test -e {}", matches[0].value);
        assert_eq!(shell.run_script(&script), ExitStatus::SUCCESS.code());
    }

    #[test]
    fn command_completion_uses_shell_path_instead_of_process_path() {
        let temp = tempfile::tempdir().unwrap();
        let shell_bin = temp.path().join("shell-bin");
        let process_bin = temp.path().join("process-bin");
        fs::create_dir(&shell_bin).unwrap();
        fs::create_dir(&process_bin).unwrap();
        executable(&shell_bin.join("shell-only"));
        executable(&process_bin.join("process-only"));
        let mut shell = Shell::builder()
            .cwd(temp.path().to_path_buf())
            .env_clear()
            .build();
        shell.set_var("PATH", shell_bin.to_str().unwrap()).unwrap();
        let context = CompletionContext::from_shell(&shell);

        assert_eq!(values(complete("shell", 5, &context)), vec!["shell-only"]);
        assert!(complete("process", 7, &context).is_empty());
    }

    #[test]
    fn hidden_files_are_only_suggested_for_dot_prefixes() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(".secret"), "").unwrap();
        fs::write(temp.path().join("visible"), "").unwrap();
        let context = context(temp.path(), vec![]);

        assert_eq!(values(complete("cat ", 4, &context)), vec!["visible"]);
        assert_eq!(values(complete("cat .", 5, &context)), vec![".secret"]);
    }

    #[test]
    fn variable_completion_handles_plain_and_braced_names() {
        let temp = tempfile::tempdir().unwrap();
        let context = context(temp.path(), vec![]);

        assert_eq!(
            values(complete("echo $PROJ", 10, &context)),
            vec!["PROJECT_ROOT"]
        );
        assert_eq!(
            values(complete("echo ${PROJ", 11, &context)),
            vec!["PROJECT_ROOT}"]
        );
    }

    #[test]
    fn completion_context_snapshots_shell_path_functions_variables_and_embedders() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        executable(&bin.join("unique-tool"));
        let mut shell = Shell::builder()
            .cwd(temp.path().to_path_buf())
            .env_clear()
            .build();
        shell.set_var("PATH", bin.to_str().unwrap()).unwrap();
        shell.set_var("PROJECT_ROOT", "/project").unwrap();
        shell.run_script("greet() { :; }");
        shell
            .register_embedder_builtin("host_job", |_shell, _argv| Ok(ExitStatus::SUCCESS))
            .unwrap();

        let context = CompletionContext::from_shell(&shell);
        assert!(context.resolves_command("greet"));
        assert!(context.resolves_command("host_job"));
        assert!(context.variables.contains(&"PROJECT_ROOT".to_owned()));
        assert_eq!(values(complete("uni", 3, &context)), vec!["unique-tool"]);
    }
}

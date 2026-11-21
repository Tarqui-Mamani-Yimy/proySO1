//! Opt-in command and write policy for the interactive shell (`SO1SH_POLICY`).
//!
//! The policy file holds one rule per line; `#` starts a comment and blank
//! lines are ignored. Exactly one `default allow` or `default deny` line is
//! required. `allow|deny command NAME...` rules match `argv[0]` or its
//! basename; `allow|deny write GLOB...` rules match the lexically normalized
//! absolute target of a writable redirection with `epsh::glob::fnmatch`.
//! Command and write rules each use their own first-match order, in file
//! order, and fall back to the default. This is not a sandbox: programs that
//! run other programs, symlinks in write targets, and reads are not policed.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use epsh::eval::{CommandPolicyDecision, RedirectOpen, Shell};
use epsh::shell_bytes::ShellBytes;

/// What a rule or the default does with a matching command or write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Allow,
    Deny,
}

/// A parsed policy file.
#[derive(Debug, PartialEq, Eq)]
pub struct Policy {
    default: Action,
    commands: Vec<(Action, Vec<String>)>,
    writes: Vec<(Action, Vec<String>)>,
}

/// A policy file parse failure; `line` is 1-based, `None` for whole-file errors.
#[derive(Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: Option<usize>,
    pub message: String,
}

pub fn parse(source: &str) -> Result<Policy, ParseError> {
    let mut default = None;
    let mut commands = Vec::new();
    let mut writes = Vec::new();
    for (index, raw) in source.lines().enumerate() {
        let line = index + 1;
        let fail = |message: String| ParseError {
            line: Some(line),
            message,
        };
        let text = raw.split_once('#').map_or(raw, |(rule, _)| rule);
        let mut words = text.split_whitespace();
        let Some(directive) = words.next() else {
            continue;
        };
        let action = match directive {
            "allow" => Action::Allow,
            "deny" => Action::Deny,
            "default" => {
                let value = match (words.next(), words.next()) {
                    (Some("allow"), None) => Action::Allow,
                    (Some("deny"), None) => Action::Deny,
                    _ => return Err(fail("default must be `allow` or `deny`".into())),
                };
                if default.replace(value).is_some() {
                    return Err(fail("duplicate default rule".into()));
                }
                continue;
            }
            other => return Err(fail(format!("unknown rule `{other}`"))),
        };
        let (rules, kind, operand) = match words.next() {
            Some("command") => (&mut commands, "command", "name"),
            Some("write") => (&mut writes, "write", "pattern"),
            Some(kind) => {
                return Err(fail(format!(
                    "unknown rule kind `{kind}`; expected `command` or `write`"
                )));
            }
            None => {
                return Err(fail(format!(
                    "`{directive}` needs a rule kind: `command` or `write`"
                )));
            }
        };
        let operands: Vec<String> = words.map(str::to_owned).collect();
        if operands.is_empty() {
            return Err(fail(format!(
                "`{directive} {kind}` needs at least one {operand}"
            )));
        }
        rules.push((action, operands));
    }
    let default = default.ok_or_else(|| ParseError {
        line: None,
        message: "missing `default allow` or `default deny`".into(),
    })?;
    Ok(Policy {
        default,
        commands,
        writes,
    })
}

impl Policy {
    /// Decide a command by its `argv[0]` bytes.
    pub fn command_action(&self, argv0: &[u8]) -> Action {
        let basename = argv0.rsplit(|&byte| byte == b'/').next().unwrap_or(argv0);
        self.commands
            .iter()
            .find(|(_, names)| {
                names
                    .iter()
                    .any(|name| name.as_bytes() == argv0 || name.as_bytes() == basename)
            })
            .map_or(self.default, |(action, _)| *action)
    }

    /// Decide a write to an absolute, lexically normalized target.
    pub fn write_action(&self, target: &Path) -> Action {
        use std::os::unix::ffi::OsStrExt;
        let target = epsh::encoding::bytes_to_str(target.as_os_str().as_bytes());
        self.writes
            .iter()
            .find(|(_, patterns)| {
                patterns
                    .iter()
                    .any(|pattern| epsh::glob::fnmatch(pattern, &target))
            })
            .map_or(self.default, |(action, _)| *action)
    }
}

/// Resolve `target` against `cwd` and remove `.` and `..` lexically.
pub fn normalize_target(cwd: &Path, target: &Path) -> PathBuf {
    let mut normalized = PathBuf::from("/");
    for component in cwd.join(target).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    normalized
}

/// Read and parse the policy file, formatting failures as one diagnostic.
pub fn load(path: &Path) -> Result<Policy, String> {
    let shown = path.display();
    let source = std::fs::read(path).map_err(|error| format!("so1sh: policy {shown}: {error}"))?;
    let source = String::from_utf8(source)
        .map_err(|_| format!("so1sh: policy {shown}: file is not valid UTF-8"))?;
    parse(&source).map_err(|error| match error.line {
        Some(line) => format!("so1sh: policy {shown}:{line}: {}", error.message),
        None => format!("so1sh: policy {shown}: {}", error.message),
    })
}

/// Enforce `policy` on `shell`: commands through the epsh command policy hook
/// and writable redirections through the redirect open provider.
pub fn install(shell: &mut Shell, policy: Policy) {
    let policy = Arc::new(policy);
    let commands = Arc::clone(&policy);
    shell.set_command_policy(Box::new(move |argv: &[ShellBytes]| {
        match commands.command_action(argv[0].as_bytes()) {
            Action::Allow => CommandPolicyDecision::Allow,
            Action::Deny => CommandPolicyDecision::Deny(String::new()),
        }
    }));
    shell.set_redirect_open_handler(Box::new(move |request: &RedirectOpen<'_>| {
        // epsh already resolves targets against the shell cwd; the process cwd
        // only backs a target that is somehow still relative.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        match policy.write_action(&normalize_target(&cwd, request.path)) {
            Action::Allow => None,
            Action::Deny => Some(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "denied by policy",
            ))),
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(source: &str) -> ParseError {
        parse(source).unwrap_err()
    }

    #[test]
    fn default_is_required() {
        assert_eq!(
            err("allow command ls\n"),
            ParseError {
                line: None,
                message: "missing `default allow` or `default deny`".into(),
            }
        );
    }

    #[test]
    fn duplicate_default_names_its_line() {
        assert_eq!(
            err("default allow\n\ndefault deny\n"),
            ParseError {
                line: Some(3),
                message: "duplicate default rule".into(),
            }
        );
    }

    #[test]
    fn unknown_directive_names_its_line() {
        assert_eq!(
            err("default deny\npermit command ls\n"),
            ParseError {
                line: Some(2),
                message: "unknown rule `permit`".into(),
            }
        );
        assert_eq!(
            err("default deny\nallow read /etc/*\n"),
            ParseError {
                line: Some(2),
                message: "unknown rule kind `read`; expected `command` or `write`".into(),
            }
        );
        assert_eq!(
            err("default maybe\n"),
            ParseError {
                line: Some(1),
                message: "default must be `allow` or `deny`".into(),
            }
        );
        assert_eq!(
            err("default deny\nallow command\n"),
            ParseError {
                line: Some(2),
                message: "`allow command` needs at least one name".into(),
            }
        );
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let policy =
            parse("# header\n\n   \ndefault deny # trailing\nallow command ls # list\n").unwrap();
        assert_eq!(policy.command_action(b"ls"), Action::Allow);
        assert_eq!(policy.command_action(b"rm"), Action::Deny);
    }

    #[test]
    fn first_matching_command_rule_wins() {
        let policy = parse("default allow\ndeny command rm\nallow command rm cp\n").unwrap();
        assert_eq!(policy.command_action(b"rm"), Action::Deny);
        assert_eq!(policy.command_action(b"cp"), Action::Allow);
        assert_eq!(policy.command_action(b"mv"), Action::Allow);
        let policy = parse("default deny\nallow command rm\ndeny command rm\n").unwrap();
        assert_eq!(policy.command_action(b"rm"), Action::Allow);
    }

    #[test]
    fn command_names_match_argv0_or_its_basename() {
        let policy = parse("default allow\ndeny command rm /opt/tool\n").unwrap();
        assert_eq!(policy.command_action(b"/bin/rm"), Action::Deny);
        assert_eq!(policy.command_action(b"./rm"), Action::Deny);
        assert_eq!(policy.command_action(b"/opt/tool"), Action::Deny);
        assert_eq!(policy.command_action(b"tool"), Action::Allow);
        assert_eq!(policy.command_action(b"rmdir"), Action::Allow);
    }

    #[test]
    fn write_rules_use_their_own_first_match_order() {
        let policy = parse(
            "default deny\nallow command touch\nallow write /tmp/keep/* /dev/null\ndeny write /tmp/*\nallow write /tmp/x\n",
        )
        .unwrap();
        assert_eq!(policy.write_action(Path::new("/tmp/keep/a")), Action::Allow);
        assert_eq!(policy.write_action(Path::new("/dev/null")), Action::Allow);
        assert_eq!(policy.write_action(Path::new("/tmp/x")), Action::Deny);
        assert_eq!(policy.write_action(Path::new("/home/u/f")), Action::Deny);
        // Command rules never decide writes.
        assert_eq!(policy.write_action(Path::new("touch")), Action::Deny);
        let policy = parse("default allow\ndeny write /etc/*\n").unwrap();
        assert_eq!(policy.write_action(Path::new("/etc/passwd")), Action::Deny);
        assert_eq!(policy.write_action(Path::new("/home/u/f")), Action::Allow);
    }

    #[test]
    fn relative_write_targets_are_normalized_against_cwd() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            normalize_target(cwd, Path::new("out.txt")),
            PathBuf::from("/work/project/out.txt")
        );
        assert_eq!(
            normalize_target(cwd, Path::new("./a/../../secret")),
            PathBuf::from("/work/secret")
        );
        assert_eq!(
            normalize_target(cwd, Path::new("/x/./y/../z")),
            PathBuf::from("/x/z")
        );
        assert_eq!(
            normalize_target(cwd, Path::new("/../../etc")),
            PathBuf::from("/etc")
        );
    }
}

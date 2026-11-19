//! Prompt expansion and the default so1sh prompt (see odd/tasks/shell-prompt-banner.md, T1).
//!
//! An explicitly set `PS1`/`PS2` keeps full user control: it undergoes POSIX
//! parameter expansion through [`Shell::expand_prompt`] before each prompt and
//! falls back to its literal value when expansion fails. Only an unset `PS1`
//! selects the default prompt `user@host <cwd> (<git branch>) $ `.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use epsh::eval::Shell;
use epsh::shell_bytes::ShellBytes;
use nu_ansi_term::{Color, Style};

const DEFAULT_PS2: &str = "> ";

/// Inputs of the default prompt, gathered from the shell and the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DefaultPromptInfo {
    pub(crate) user: String,
    pub(crate) host: String,
    pub(crate) cwd: String,
    pub(crate) git_branch: Option<String>,
    pub(crate) root: bool,
    pub(crate) failed: bool,
}

/// The primary prompt for the next line.
pub(crate) fn primary(shell: &mut Shell) -> String {
    match shell.get_var_bytes("PS1").cloned() {
        Some(ps1) => expand_or_literal(shell, &ps1),
        None => {
            let color = color_enabled(shell, std::io::stdout().is_terminal());
            render_default(&default_info(shell), color)
        }
    }
}

/// The continuation prompt (`PS2`, default `> `).
pub(crate) fn continuation(shell: &mut Shell) -> String {
    match shell.get_var_bytes("PS2").cloned() {
        Some(ps2) => expand_or_literal(shell, &ps2),
        None => DEFAULT_PS2.to_owned(),
    }
}

/// Reedline's Prompt requires str: invalid UTF-8 is replaced for display only.
/// An expansion error shows the literal value and keeps the session usable.
fn expand_or_literal(shell: &mut Shell, value: &ShellBytes) -> String {
    let shown = shell.expand_prompt(value);
    let bytes = shown.as_ref().unwrap_or(value);
    String::from_utf8_lossy(bytes.as_bytes()).into_owned()
}

/// Colors are used only when stdout is a terminal and `NO_COLOR` is unset or empty.
pub(crate) fn color_enabled(shell: &Shell, stdout_is_terminal: bool) -> bool {
    stdout_is_terminal && shell.get_var_bytes("NO_COLOR").is_none_or(|v| v.is_empty())
}

pub(crate) fn render_default(info: &DefaultPromptInfo, color: bool) -> String {
    let paint = |style: Style, text: &str| {
        if color {
            style.paint(text).to_string()
        } else {
            text.to_owned()
        }
    };
    let mut prompt = paint(
        Style::new().bold().fg(Color::Green),
        &format!("{}@{}", info.user, info.host),
    );
    prompt.push(' ');
    prompt.push_str(&paint(Style::new().bold().fg(Color::Blue), &info.cwd));
    if let Some(branch) = &info.git_branch {
        prompt.push(' ');
        prompt.push_str(&paint(
            Style::new().fg(Color::Yellow),
            &format!("({branch})"),
        ));
    }
    prompt.push(' ');
    let marker = if info.root { "#" } else { "$" };
    if info.failed {
        prompt.push_str(&paint(Style::new().bold().fg(Color::Red), marker));
    } else {
        prompt.push_str(marker);
    }
    prompt.push(' ');
    prompt
}

fn default_info(shell: &Shell) -> DefaultPromptInfo {
    let home = shell
        .get_var_bytes("HOME")
        .filter(|v| !v.is_empty())
        .map(ShellBytes::to_path_buf);
    DefaultPromptInfo {
        user: user_name(shell),
        host: host_name(),
        cwd: abbreviate_home(shell.cwd(), home.as_deref()),
        git_branch: git_branch(shell.cwd()),
        // SAFETY: geteuid has no preconditions and cannot fail.
        root: unsafe { libc::geteuid() } == 0,
        failed: shell.exit_status().code() != 0,
    }
}

/// Show `cwd` with a leading `$HOME` replaced by `~` (component-wise).
pub(crate) fn abbreviate_home(cwd: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home.filter(|home| home.parent().is_some())
        && let Ok(rest) = cwd.strip_prefix(home)
    {
        if rest.as_os_str().is_empty() {
            return "~".to_owned();
        }
        return format!("~/{}", rest.to_string_lossy());
    }
    cwd.to_string_lossy().into_owned()
}

/// The current branch, read from `.git/HEAD` in `cwd` or its nearest ancestor
/// that has `.git` (no git process). A worktree or submodule `.git` file that
/// points at a `gitdir:` is followed. Detached HEAD shows the short commit.
pub(crate) fn git_branch(cwd: &Path) -> Option<String> {
    let dot_git = cwd
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|path| path.exists())?;
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let contents = read_small(&dot_git)?;
        let target = contents.strip_prefix("gitdir:")?.trim();
        let parent = dot_git.parent()?;
        parent.join(PathBuf::from(target))
    };
    let head = read_small(&git_dir.join("HEAD"))?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref:") {
        let reference = reference.trim();
        let branch = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        return (!branch.is_empty()).then(|| branch.to_owned());
    }
    (head.len() >= 7 && head.bytes().all(|b| b.is_ascii_hexdigit())).then(|| head[..7].to_owned())
}

/// Read a short metadata file; anything larger than 4 KiB is not a HEAD/gitdir file.
fn read_small(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut contents = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(4096)
        .read_to_string(&mut contents)
        .ok()?;
    Some(contents)
}

/// The effective user's login name, falling back to `$USER`, then the uid.
fn user_name(shell: &Shell) -> String {
    // SAFETY: geteuid has no preconditions; getpwuid_r writes only into the
    // provided buffers, and pw_name is read while `buffer` is alive.
    unsafe {
        let uid = libc::geteuid();
        let mut passwd: libc::passwd = std::mem::zeroed();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0 as libc::c_char; 4096];
        if libc::getpwuid_r(
            uid,
            &mut passwd,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        ) == 0
            && !result.is_null()
            && !passwd.pw_name.is_null()
        {
            let name = std::ffi::CStr::from_ptr(passwd.pw_name).to_string_lossy();
            if !name.is_empty() {
                return name.into_owned();
            }
        }
        shell
            .get_var_bytes("USER")
            .filter(|v| !v.is_empty())
            .map_or_else(|| uid.to_string(), |v| v.to_string_lossy().into_owned())
    }
}

/// The host name up to its first dot, like `\h` in other shells.
fn host_name() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: gethostname writes at most `buffer.len()` bytes into `buffer`.
    let ok = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
    if !ok {
        return "localhost".to_owned();
    }
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]);
    let short = name.split('.').next().unwrap_or_default();
    if short.is_empty() {
        "localhost".to_owned()
    } else {
        short.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> DefaultPromptInfo {
        DefaultPromptInfo {
            user: "ana".to_owned(),
            host: "box".to_owned(),
            cwd: "~/src".to_owned(),
            git_branch: None,
            root: false,
            failed: false,
        }
    }

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn default_prompt_without_color_has_user_host_cwd_and_dollar() {
        assert_eq!(render_default(&info(), false), "ana@box ~/src $ ");
        let branch = DefaultPromptInfo {
            git_branch: Some("main".to_owned()),
            ..info()
        };
        assert_eq!(render_default(&branch, false), "ana@box ~/src (main) $ ");
        let root = DefaultPromptInfo {
            root: true,
            failed: true,
            ..info()
        };
        assert_eq!(render_default(&root, false), "ana@box ~/src # ");
    }

    #[test]
    fn default_prompt_marks_a_failed_status_in_red_only_with_color() {
        let ok = render_default(&info(), true);
        assert!(ok.ends_with(" $ "), "{ok:?}");
        assert!(ok.contains("\x1b["), "{ok:?}");
        let failed = render_default(
            &DefaultPromptInfo {
                failed: true,
                ..info()
            },
            true,
        );
        let red_marker = Style::new().bold().fg(Color::Red).paint("$").to_string();
        assert!(failed.ends_with(&format!("{red_marker} ")), "{failed:?}");
        assert!(!render_default(&info(), false).contains('\x1b'));
    }

    #[test]
    fn color_rule_requires_terminal_and_unset_or_empty_no_color() {
        let mut shell = Shell::builder().env_clear().build();
        assert!(color_enabled(&shell, true));
        assert!(!color_enabled(&shell, false));
        shell.set_var("NO_COLOR", "").unwrap();
        assert!(color_enabled(&shell, true));
        shell.set_var("NO_COLOR", "1").unwrap();
        assert!(!color_enabled(&shell, true));
    }

    #[test]
    fn home_is_abbreviated_to_tilde_by_whole_components() {
        let home = Path::new("/home/ana");
        assert_eq!(abbreviate_home(Path::new("/home/ana"), Some(home)), "~");
        assert_eq!(
            abbreviate_home(Path::new("/home/ana/src/x"), Some(home)),
            "~/src/x"
        );
        assert_eq!(
            abbreviate_home(Path::new("/home/anabel"), Some(home)),
            "/home/anabel"
        );
        assert_eq!(abbreviate_home(Path::new("/tmp"), None), "/tmp");
        assert_eq!(
            abbreviate_home(Path::new("/usr"), Some(Path::new("/"))),
            "/usr"
        );
    }

    #[test]
    fn git_branch_is_read_from_head_in_cwd_or_an_ancestor() {
        let dir = scratch();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("a/b")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        assert_eq!(git_branch(&repo), Some("feat/x".to_owned()));
        assert_eq!(git_branch(&repo.join("a/b")), Some("feat/x".to_owned()));
        assert_eq!(git_branch(dir.path()), None);
    }

    #[test]
    fn detached_head_shows_the_short_commit() {
        let dir = scratch();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(git_branch(dir.path()), Some("0123456".to_owned()));
        std::fs::write(dir.path().join(".git/HEAD"), "garbage\n").unwrap();
        assert_eq!(git_branch(dir.path()), None);
    }

    #[test]
    fn worktree_git_file_points_at_its_gitdir() {
        let dir = scratch();
        let gitdir = dir.path().join("main/.git/worktrees/wt");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/wt-branch\n").unwrap();
        let worktree = dir.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
        assert_eq!(git_branch(&worktree), Some("wt-branch".to_owned()));
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        assert_eq!(git_branch(&worktree), Some("wt-branch".to_owned()));
    }

    #[test]
    fn explicit_ps1_is_expanded_and_falls_back_to_its_literal_value() {
        let mut shell = Shell::builder().env_clear().build();
        shell.set_var("PS1", "$PWD> ").unwrap();
        shell.set_var("PWD", "/w").unwrap();
        assert_eq!(primary(&mut shell), "/w> ");
        shell.set_var("PS1", "${x?boom}> ").unwrap();
        assert_eq!(primary(&mut shell), "${x?boom}> ");
        shell.set_var("PS1", "plain $ ").unwrap();
        assert_eq!(primary(&mut shell), "plain $ ");
        assert_eq!(continuation(&mut shell), "> ");
        shell.set_var("PS2", "${PWD}+ ").unwrap();
        assert_eq!(continuation(&mut shell), "/w+ ");
    }
}

#[cfg(unix)]
fn main() {
    if let Err(error) = run() {
        eprintln!("so1ctl: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("so1ctl: local control is unsupported on Windows in v1");
    std::process::exit(1);
}

#[cfg(unix)]
fn run() -> Result<(), String> {
    use so1_terminal::control::{self, ControlOutcome, ControlRequest};
    use std::{
        env,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static REQUEST_ID: AtomicU64 = AtomicU64::new(1);
    let mut args = env::args().skip(1).peekable();
    let mut socket = None;
    while args.peek().is_some_and(|arg| arg == "--socket") {
        args.next();
        socket = Some(PathBuf::from(
            args.next().ok_or("--socket requires a path")?,
        ));
    }
    if args
        .peek()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!(
            "Usage: so1ctl [--socket PATH] <command> [args]\nCommands: list, new-tab, split --direction right|down [--pane ID], focus --pane ID, close --pane ID, send-text [--pane ID] TEXT, get-text [--pane ID] [--scope screen|last-block], block [--pane ID] [--id N], wait [--pane ID] [--after ID] [--timeout SECS] (default 600), shutdown (so1term-server only)"
        );
        return Ok(());
    }
    let command_name = args.next().ok_or("missing command (try --help)")?;
    if command_name == "wait" {
        let wait = parse_wait(args.collect())?;
        let socket = resolve_socket(socket)?;
        return wait_for_block(&socket, &wait);
    }
    let command = parse_command(&command_name, args.collect())?;
    let socket = resolve_socket(socket)?;
    let request = ControlRequest {
        id: REQUEST_ID.fetch_add(1, Ordering::Relaxed),
        command,
    };
    let response = control::send_request(&socket, &request).map_err(|error| error.to_string())?;
    match response.outcome {
        ControlOutcome::Success(value) => println!(
            "{}",
            serde_json::to_string(&value).map_err(|error| error.to_string())?
        ),
        ControlOutcome::Failure(error) => {
            return Err(format!("{}: {}", error.code.as_str(), error.message));
        }
    }
    Ok(())
}

/// Picks the control socket: `--socket`, then `SO1TERM_SOCKET`, then the only
/// so1term socket in the runtime directory.
#[cfg(unix)]
fn resolve_socket(socket: Option<std::path::PathBuf>) -> Result<std::path::PathBuf, String> {
    use so1_terminal::control;
    use std::{env, path::PathBuf};
    let path = if let Some(path) =
        socket.or_else(|| env::var_os("SO1TERM_SOCKET").map(PathBuf::from))
    {
        path
    } else {
        let directory = control::runtime_directory().map_err(|error| error.to_string())?;
        let entries = std::fs::read_dir(directory)
            .map_err(|error| format!("cannot find control socket: {error}"))?;
        let mut sockets = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("so1term-") && name.ends_with(".sock"))
            })
            .collect::<Vec<_>>();
        sockets.sort();
        match sockets.len() {
            0 => return Err("no so1term socket found; start so1term or set SO1TERM_SOCKET".into()),
            1 => sockets.remove(0),
            _ => {
                return Err(
                    "multiple so1term sockets found; set --socket or SO1TERM_SOCKET".into(),
                );
            }
        }
    };
    Ok(path)
}

/// Arguments of the client-side `wait` command.
#[cfg(unix)]
#[derive(Debug, PartialEq)]
struct WaitArgs {
    pane: Option<u64>,
    /// Only a block with a larger id counts, so a block that finished before
    /// the awaited command started is never returned.
    after: Option<u64>,
    timeout: std::time::Duration,
}

#[cfg(unix)]
fn parse_wait(args: Vec<String>) -> Result<WaitArgs, String> {
    let mut wait = WaitArgs {
        pane: None,
        after: None,
        timeout: std::time::Duration::from_secs(600),
    };
    let (mut pane_seen, mut after_seen, mut timeout_seen) = (false, false, false);
    let mut index = 0;
    while index < args.len() {
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{} requires a value", args[index]));
        match args[index].as_str() {
            "--pane" if !pane_seen => {
                pane_seen = true;
                wait.pane = Some(
                    value?
                        .parse()
                        .map_err(|_| "--pane must be an unsigned integer")?,
                );
            }
            "--after" if !after_seen => {
                after_seen = true;
                wait.after = Some(
                    value?
                        .parse()
                        .map_err(|_| "--after must be an unsigned integer")?,
                );
            }
            "--timeout" if !timeout_seen => {
                timeout_seen = true;
                let seconds = value?
                    .parse::<f64>()
                    .ok()
                    .and_then(|seconds| std::time::Duration::try_from_secs_f64(seconds).ok())
                    .ok_or("--timeout must be a non-negative number of seconds")?;
                wait.timeout = seconds;
            }
            "--pane" | "--after" | "--timeout" => {
                return Err(format!("{} may only be supplied once", args[index]));
            }
            arg => return Err(format!("unexpected argument for wait: {arg}")),
        }
        index += 2;
    }
    Ok(wait)
}

/// Polls the `block` command until the pane's newest command block (newer
/// than `--after`, when given) has finished, then prints it.
#[cfg(unix)]
fn wait_for_block(socket: &std::path::Path, wait: &WaitArgs) -> Result<(), String> {
    use so1_terminal::control::{
        self, ControlCommand, ControlErrorCode, ControlOutcome, ControlRequest,
    };
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + wait.timeout;
    let mut id = 1;
    loop {
        let request = ControlRequest {
            id,
            command: ControlCommand::Block {
                pane: wait.pane,
                block: None,
            },
        };
        id += 1;
        let response =
            control::send_request(socket, &request).map_err(|error| error.to_string())?;
        match response.outcome {
            ControlOutcome::Success(block) => {
                let newer = wait
                    .after
                    .is_none_or(|after| block["id"].as_u64().is_some_and(|id| id > after));
                if newer && block["state"] == "finished" {
                    println!(
                        "{}",
                        serde_json::to_string(&block).map_err(|error| error.to_string())?
                    );
                    return Ok(());
                }
            }
            // No command has run yet: keep waiting for the first one.
            ControlOutcome::Failure(error)
                if error.code == ControlErrorCode::NotFound
                    && error.message.ends_with("has not run a command") => {}
            ControlOutcome::Failure(error) => {
                return Err(format!("{}: {}", error.code.as_str(), error.message));
            }
        }
        if Instant::now() >= deadline {
            let what = wait.after.map_or("no command block".to_owned(), |after| {
                format!("no block newer than {after}")
            });
            return Err(format!(
                "TIMEOUT: {what} finished within {}s",
                wait.timeout.as_secs_f64()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn parse_command(
    command_name: &str,
    args: Vec<String>,
) -> Result<so1_terminal::control::ControlCommand, String> {
    use so1_terminal::control::{ControlCommand, ControlDirection, TextScope};
    let (allow_pane, allow_direction, allow_scope, allow_text, allow_id) = match command_name {
        "list" | "new-tab" | "shutdown" => (false, false, false, false, false),
        "split" => (true, true, false, false, false),
        "focus" | "close" => (true, false, false, false, false),
        "send-text" => (true, false, false, true, false),
        "get-text" => (true, false, true, false, false),
        "block" => (true, false, false, false, true),
        _ => return Err(format!("unknown command: {command_name}")),
    };
    let mut pane = None;
    let mut direction = None;
    let mut scope = TextScope::Screen;
    let mut scope_seen = false;
    let mut text = None;
    let mut block = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let next = |option: &str| {
            args.get(index + 1)
                .ok_or_else(|| format!("{option} requires a value"))
        };
        match arg.as_str() {
            "--pane" if allow_pane => {
                if pane.is_some() {
                    return Err("--pane may only be supplied once".into());
                }
                pane = Some(
                    next("--pane")?
                        .parse::<u64>()
                        .map_err(|_| "--pane must be an unsigned integer")?,
                );
                index += 2;
            }
            "--direction" if allow_direction => {
                if direction.is_some() {
                    return Err("--direction may only be supplied once".into());
                }
                direction = Some(match next("--direction")?.as_str() {
                    "right" => ControlDirection::Right,
                    "down" => ControlDirection::Down,
                    _ => return Err("--direction must be right or down".into()),
                });
                index += 2;
            }
            "--scope" if allow_scope => {
                if scope_seen {
                    return Err("--scope may only be supplied once".into());
                }
                scope_seen = true;
                scope = match next("--scope")?.as_str() {
                    "screen" => TextScope::Screen,
                    "last-block" => TextScope::LastBlock,
                    _ => return Err("--scope must be screen or last-block".into()),
                };
                index += 2;
            }
            "--id" if allow_id => {
                if block.is_some() {
                    return Err("--id may only be supplied once".into());
                }
                block = Some(
                    next("--id")?
                        .parse::<u64>()
                        .map_err(|_| "--id must be an unsigned integer")?,
                );
                index += 2;
            }
            _ if allow_text && text.is_none() => {
                text = Some(arg.clone());
                index += 1;
            }
            _ => return Err(format!("unexpected argument for {command_name}: {arg}")),
        }
    }
    match command_name {
        "list" => Ok(ControlCommand::List),
        "new-tab" => Ok(ControlCommand::NewTab),
        "split" => Ok(ControlCommand::Split {
            pane,
            direction: direction.ok_or("split requires --direction right|down")?,
        }),
        "focus" => Ok(ControlCommand::Focus {
            pane: pane.ok_or("focus requires --pane ID")?,
        }),
        "close" => Ok(ControlCommand::Close {
            pane: pane.ok_or("close requires --pane ID")?,
        }),
        "send-text" => Ok(ControlCommand::SendText {
            pane,
            text: text.ok_or("send-text requires TEXT")?,
        }),
        "get-text" => Ok(ControlCommand::GetText { pane, scope }),
        "block" => Ok(ControlCommand::Block { pane, block }),
        "shutdown" => Ok(ControlCommand::Shutdown),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod wait_tests {
    #[cfg(unix)]
    #[test]
    fn wait_arguments_parse_pane_after_and_timeout() {
        use super::{WaitArgs, parse_wait};
        use std::time::Duration;
        assert_eq!(
            parse_wait(vec![]),
            Ok(WaitArgs {
                pane: None,
                after: None,
                timeout: Duration::from_secs(600),
            })
        );
        assert_eq!(
            parse_wait(
                ["--pane", "3", "--after", "7", "--timeout", "1.5"]
                    .map(String::from)
                    .to_vec()
            ),
            Ok(WaitArgs {
                pane: Some(3),
                after: Some(7),
                timeout: Duration::from_millis(1500),
            })
        );
        for args in [
            vec!["--after", "x"],
            vec!["--timeout", "-1"],
            vec!["--timeout", "nan"],
            vec!["--pane", "1", "--pane", "2"],
            vec!["--scope", "screen"],
        ] {
            assert!(
                parse_wait(args.iter().map(|arg| arg.to_string()).collect()).is_err(),
                "{args:?}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn cli_argument_validation_matches_each_command_shape() {
        use super::parse_command;
        use so1_terminal::control::{ControlCommand, ControlDirection, TextScope};
        let cases = [
            ("list", vec![], Ok(ControlCommand::List)),
            ("new-tab", vec![], Ok(ControlCommand::NewTab)),
            ("shutdown", vec![], Ok(ControlCommand::Shutdown)),
            (
                "split",
                vec!["--direction".into(), "right".into()],
                Ok(ControlCommand::Split {
                    pane: None,
                    direction: ControlDirection::Right,
                }),
            ),
            (
                "focus",
                vec!["--pane".into(), "4".into()],
                Ok(ControlCommand::Focus { pane: 4 }),
            ),
            (
                "close",
                vec!["--pane".into(), "4".into()],
                Ok(ControlCommand::Close { pane: 4 }),
            ),
            (
                "send-text",
                vec!["--pane".into(), "4".into(), "echo hi\n".into()],
                Ok(ControlCommand::SendText {
                    pane: Some(4),
                    text: "echo hi\n".into(),
                }),
            ),
            (
                "get-text",
                vec!["--scope".into(), "last-block".into()],
                Ok(ControlCommand::GetText {
                    pane: None,
                    scope: TextScope::LastBlock,
                }),
            ),
            (
                "block",
                vec![],
                Ok(ControlCommand::Block {
                    pane: None,
                    block: None,
                }),
            ),
            (
                "block",
                vec!["--pane".into(), "2".into(), "--id".into(), "5".into()],
                Ok(ControlCommand::Block {
                    pane: Some(2),
                    block: Some(5),
                }),
            ),
        ];
        for (name, args, expected) in cases {
            assert_eq!(parse_command(name, args), expected, "{name}");
        }
        for (name, args) in [
            ("block", vec!["--id".into(), "x".into()]),
            (
                "block",
                vec!["--id".into(), "1".into(), "--id".into(), "2".into()],
            ),
            ("get-text", vec!["--id".into(), "1".into()]),
            ("new-tab", vec!["--pane".into(), "1".into()]),
            ("close", vec!["--scope".into(), "screen".into()]),
            ("list", vec!["extra".into()]),
        ] {
            assert!(parse_command(name, args).is_err(), "{name}");
        }
    }
}

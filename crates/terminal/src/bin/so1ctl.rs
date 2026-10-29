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
            "Usage: so1ctl [--socket PATH] <command> [args]\nCommands: list, new-tab, split --direction right|down [--pane ID], focus --pane ID, close --pane ID, send-text [--pane ID] TEXT, get-text [--pane ID] [--scope screen|last-block]"
        );
        return Ok(());
    }
    let command_name = args.next().ok_or("missing command (try --help)")?;
    let command = parse_command(&command_name, args.collect())?;
    let socket = if let Some(path) =
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

#[cfg(unix)]
fn parse_command(
    command_name: &str,
    args: Vec<String>,
) -> Result<so1_terminal::control::ControlCommand, String> {
    use so1_terminal::control::{ControlCommand, ControlDirection, TextScope};
    let (allow_pane, allow_direction, allow_scope, allow_text) = match command_name {
        "list" | "new-tab" => (false, false, false, false),
        "split" => (true, true, false, false),
        "focus" | "close" => (true, false, false, false),
        "send-text" => (true, false, false, true),
        "get-text" => (true, false, true, false),
        _ => return Err(format!("unknown command: {command_name}")),
    };
    let mut pane = None;
    let mut direction = None;
    let mut scope = TextScope::Screen;
    let mut scope_seen = false;
    let mut text = None;
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
        _ => unreachable!(),
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
        ];
        for (name, args, expected) in cases {
            assert_eq!(parse_command(name, args), expected, "{name}");
        }
        for (name, args) in [
            ("new-tab", vec!["--pane".into(), "1".into()]),
            ("close", vec!["--scope".into(), "screen".into()]),
            ("list", vec!["extra".into()]),
        ] {
            assert!(parse_command(name, args).is_err(), "{name}");
        }
    }
}

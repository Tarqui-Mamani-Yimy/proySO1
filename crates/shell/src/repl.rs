use std::io::{self, BufRead, Write};
use std::ops::ControlFlow;

use epsh::eval::Shell;

/// Plain, one-line REPL; parsing, state, and exit semantics belong to epsh.
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
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut line = Vec::new();
    loop {
        {
            let prompt = shell
                .get_var_bytes("PS1")
                .map_or(b"$ ".as_slice(), |value| value.as_bytes());
            let mut output = io::stdout().lock();
            output.write_all(prompt)?;
            output.flush()?;
        }
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(shell.finish_repl().code());
        }
        let source = epsh::encoding::bytes_to_str(&line);
        if let ControlFlow::Break(status) = shell.eval_repl_line(&source) {
            return Ok(status.code());
        }
    }
}

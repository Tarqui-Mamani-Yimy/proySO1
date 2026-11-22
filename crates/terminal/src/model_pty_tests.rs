use crate::{model::Model, pty::Pty};
use std::{
    thread,
    time::{Duration, Instant},
};

#[test]
fn renders_prompt_bytes_from_built_shellux_pty() {
    let executable = crate::pty::test_shellux_executable();
    let executable = executable.expect(
        "shellux binary required for real PTY prompt test; run `cargo build -p shellux` first",
    );
    let command = format!("SHELLUX_KITTY_KEYBOARD=0 exec '{}'", executable.display());
    let mut pty = Pty::spawn_with_args("/bin/sh", ["-c", command.as_str()], "/", 80, 24)
        .expect("spawn built shellux with kitty keyboard negotiation disabled");
    let mut model = Model::new(80, 24);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        while let Some(chunk) = pty.try_read() {
            model.feed(&chunk);
        }
        let screen: String = model
            .visible_cells()
            .iter()
            .flatten()
            .map(|cell| cell.character)
            .collect();
        if screen.contains("$ ") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "shellux prompt not rendered; screen: {screen:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    pty.write(b"exit\n").unwrap();
}

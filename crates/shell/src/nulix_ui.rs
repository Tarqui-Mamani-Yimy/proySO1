//! Learns whether the hosting nulix window is in `up` mode before each prompt.
//!
//! `nulix up` draws command blocks as cards, and shellux prints one blank line
//! before each primary prompt so consecutive cards do not touch. The window can
//! switch between `up` and `classic` at runtime, so when `NULIX_SOCKET` names
//! its control socket, shellux asks with the `ui` control command before every
//! prompt. Any failure (no socket, refusal, timeout, an error response, a
//! malformed reply) silently falls back to the startup rule `NULIX_UI=up`.

use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::path::Path;
use std::time::Duration;

/// The only bytes ever written to the window's control socket.
pub(crate) const UI_QUERY: &[u8] = b"{\"v\":1,\"id\":1,\"cmd\":\"ui\"}\n";
/// Total time one query may take, so a busy window never stalls the prompt.
pub(crate) const UI_QUERY_BUDGET: Duration = Duration::from_millis(100);
/// Longest reply line accepted from the window.
const MAX_REPLY_BYTES: usize = 4096;

/// The window's current UI mode as reported by the `ui` control command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiMode {
    Up,
    Classic,
}

/// Decides the blank line: the window's answer wins; without one, the startup
/// environment value `NULIX_UI=up` decides.
pub(crate) fn blank_line_before_prompt(
    window_mode: Option<UiMode>,
    startup_nulix_ui: Option<&OsStr>,
) -> bool {
    match window_mode {
        Some(mode) => mode == UiMode::Up,
        None => startup_nulix_ui.is_some_and(|value| value == "up"),
    }
}

/// Startup environment for the per-prompt decision, read once.
pub(crate) struct PromptSpacing {
    socket: Option<OsString>,
    startup_nulix_ui: Option<OsString>,
}

impl PromptSpacing {
    pub(crate) fn from_env() -> Self {
        Self {
            socket: std::env::var_os("NULIX_SOCKET").filter(|value| !value.is_empty()),
            startup_nulix_ui: std::env::var_os("NULIX_UI"),
        }
    }

    /// Queries the window when a socket is known, then applies the decision.
    pub(crate) fn blank_line_before_prompt(&self) -> bool {
        #[cfg(unix)]
        let window_mode = self
            .socket
            .as_deref()
            .and_then(|socket| query_window_mode(Path::new(socket), UI_QUERY_BUDGET));
        #[cfg(not(unix))]
        let window_mode = {
            let _ = &self.socket;
            None
        };
        blank_line_before_prompt(window_mode, self.startup_nulix_ui.as_deref())
    }
}

/// Sends one `ui` query over the Unix-domain socket and reads one reply line
/// within `budget`. Returns `None` on any failure; never prints diagnostics.
#[cfg(unix)]
pub(crate) fn query_window_mode(socket: &Path, budget: Duration) -> Option<UiMode> {
    use std::io::Read;
    use std::time::Instant;

    let deadline = Instant::now() + budget;
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    };
    let mut stream = unix_socket::connect(socket, deadline)?;
    stream.set_write_timeout(Some(remaining()?)).ok()?;
    unix_socket::send_all(&stream, UI_QUERY)?;
    let mut reply = Vec::new();
    let mut chunk = [0_u8; 512];
    loop {
        stream.set_read_timeout(Some(remaining()?)).ok()?;
        let read = match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        if let Some(end) = chunk[..read].iter().position(|&byte| byte == b'\n') {
            reply.extend_from_slice(&chunk[..end]);
            break;
        }
        reply.extend_from_slice(&chunk[..read]);
        if reply.len() > MAX_REPLY_BYTES {
            return None;
        }
    }
    if reply.len() > MAX_REPLY_BYTES {
        return None;
    }
    parse_ui_response(&reply)
}

/// Raw socket calls std lacks: a connect bounded by a deadline and a send that
/// cannot raise SIGPIPE when the window has already closed the connection.
#[cfg(unix)]
mod unix_socket {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::Instant;

    pub(super) fn connect(path: &Path, deadline: Instant) -> Option<UnixStream> {
        let bytes = path.as_os_str().as_bytes();
        // SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
            return None;
        }
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (slot, &byte) in address.sun_path.iter_mut().zip(bytes) {
            *slot = byte as libc::c_char;
        }
        // SAFETY: plain socket(2) call; the result is checked before use.
        let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if raw < 0 {
            return None;
        }
        // SAFETY: `raw` is a freshly created descriptor owned by nobody else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: fcntl(2) on a descriptor this function owns.
        unsafe {
            if libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                return None;
            }
            let flags = libc::fcntl(raw, libc::F_GETFL);
            if flags < 0 || libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return None;
            }
        }
        let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
            as libc::socklen_t;
        // SAFETY: `address` is a valid sockaddr_un of at least `length` bytes.
        let connected =
            unsafe { libc::connect(raw, (&raw const address).cast::<libc::sockaddr>(), length) };
        if connected != 0 {
            let error = std::io::Error::last_os_error().raw_os_error();
            if error != Some(libc::EINPROGRESS) || !wait_writable(&fd, deadline) {
                return None;
            }
        }
        // SAFETY: fcntl(2) on a descriptor this function owns.
        unsafe {
            let flags = libc::fcntl(raw, libc::F_GETFL);
            if flags < 0 || libc::fcntl(raw, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
                return None;
            }
        }
        Some(UnixStream::from(fd))
    }

    /// Waits for an in-progress connect and reports whether it succeeded.
    fn wait_writable(fd: &OwnedFd, deadline: Instant) -> bool {
        loop {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let millis = left.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the duration of the call.
            match unsafe { libc::poll(&mut poll, 1, millis) } {
                0 => return false,
                ready if ready > 0 => break,
                _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
                _ => return false,
            }
        }
        let mut error: libc::c_int = 0;
        let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: getsockopt(2) writes one c_int into `error`.
        let status = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut error).cast::<libc::c_void>(),
                &mut length,
            )
        };
        status == 0 && error == 0
    }

    /// Sends every byte, honouring the stream's write timeout, without SIGPIPE.
    pub(super) fn send_all(stream: &UnixStream, mut bytes: &[u8]) -> Option<()> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let flags = libc::MSG_NOSIGNAL;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let flags = 0;
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            let on: libc::c_int = 1;
            // SAFETY: setsockopt(2) reads one c_int from `on`.
            let status = unsafe {
                libc::setsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_NOSIGPIPE,
                    (&raw const on).cast::<libc::c_void>(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            if status != 0 {
                return None;
            }
        }
        while !bytes.is_empty() {
            // SAFETY: `bytes` is a valid buffer of `bytes.len()` bytes.
            let sent = unsafe {
                libc::send(
                    stream.as_raw_fd(),
                    bytes.as_ptr().cast::<libc::c_void>(),
                    bytes.len(),
                    flags,
                )
            };
            if sent < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return None;
            }
            bytes = &bytes[sent as usize..];
        }
        Some(())
    }
}

/// Parses a `ui` reply line: `{"v":1,"id":1,"ok":true,"result":{"mode":M}}`
/// with keys in any order. Anything else (error responses included) is `None`.
pub(crate) fn parse_ui_response(line: &[u8]) -> Option<UiMode> {
    let mut parser = JsonParser { input: line, at: 0 };
    let reply = parser.value(0)?;
    parser.skip_whitespace();
    if parser.at != line.len() {
        return None;
    }
    if reply.field("ok")? != &Json::Bool(true) || reply.field("id")? != &Json::Number(1.0) {
        return None;
    }
    match reply.field("result")?.field("mode")? {
        Json::String(mode) if mode == "up" => Some(UiMode::Up),
        Json::String(mode) if mode == "classic" => Some(UiMode::Classic),
        _ => None,
    }
}

/// Just enough JSON to read one control reply; nesting is capped.
#[derive(Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn field(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }
}

const MAX_JSON_DEPTH: usize = 16;

struct JsonParser<'a> {
    input: &'a [u8],
    at: usize,
}

impl JsonParser<'_> {
    fn skip_whitespace(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.input.get(self.at) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> Option<()> {
        self.skip_whitespace();
        (self.input.get(self.at) == Some(&byte)).then(|| self.at += 1)
    }

    fn literal(&mut self, text: &[u8], value: Json) -> Option<Json> {
        self.input[self.at..].starts_with(text).then(|| {
            self.at += text.len();
            value
        })
    }

    fn value(&mut self, depth: usize) -> Option<Json> {
        if depth > MAX_JSON_DEPTH {
            return None;
        }
        self.skip_whitespace();
        match *self.input.get(self.at)? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Json::String),
            b't' => self.literal(b"true", Json::Bool(true)),
            b'f' => self.literal(b"false", Json::Bool(false)),
            b'n' => self.literal(b"null", Json::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn object(&mut self, depth: usize) -> Option<Json> {
        self.eat(b'{')?;
        let mut fields = Vec::new();
        if self.eat(b'}').is_some() {
            return Some(Json::Object(fields));
        }
        loop {
            self.skip_whitespace();
            let key = self.string()?;
            self.eat(b':')?;
            fields.push((key, self.value(depth + 1)?));
            if self.eat(b',').is_none() {
                self.eat(b'}')?;
                return Some(Json::Object(fields));
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<Json> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        if self.eat(b']').is_some() {
            return Some(Json::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            if self.eat(b',').is_none() {
                self.eat(b']')?;
                return Some(Json::Array(items));
            }
        }
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.at;
        while let Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') = self.input.get(self.at) {
            self.at += 1;
        }
        let text = std::str::from_utf8(&self.input[start..self.at]).ok()?;
        text.parse().ok().map(Json::Number)
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.input.get(self.at..self.at + 4)?;
        self.at += 4;
        u32::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
    }

    fn string(&mut self) -> Option<String> {
        if self.input.get(self.at) != Some(&b'"') {
            return None;
        }
        self.at += 1;
        let mut text = Vec::new();
        loop {
            let byte = *self.input.get(self.at)?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(text).ok(),
                b'\\' => {
                    let escaped = *self.input.get(self.at)?;
                    self.at += 1;
                    let decoded = match escaped {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let mut code = self.hex4()?;
                            if (0xD800..0xDC00).contains(&code) {
                                if self.input.get(self.at..self.at + 2) != Some(b"\\u") {
                                    return None;
                                }
                                self.at += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return None;
                                }
                                code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                            }
                            char::from_u32(code)?
                        }
                        _ => return None,
                    };
                    text.extend_from_slice(decoded.encode_utf8(&mut [0; 4]).as_bytes());
                }
                0x00..=0x1f => return None,
                _ => text.push(byte),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_answer_decides_and_missing_answer_falls_back_to_startup_env() {
        let up = Some(OsStr::new("up"));
        let classic = Some(OsStr::new("classic"));
        assert!(blank_line_before_prompt(Some(UiMode::Up), None));
        assert!(blank_line_before_prompt(Some(UiMode::Up), classic));
        assert!(!blank_line_before_prompt(Some(UiMode::Classic), up));
        assert!(!blank_line_before_prompt(Some(UiMode::Classic), None));
        assert!(blank_line_before_prompt(None, up));
        assert!(!blank_line_before_prompt(None, classic));
        assert!(!blank_line_before_prompt(None, Some(OsStr::new(""))));
        assert!(!blank_line_before_prompt(None, None));
    }

    #[test]
    fn parses_success_replies_in_any_key_order() {
        assert_eq!(
            parse_ui_response(br#"{"v":1,"id":1,"ok":true,"result":{"mode":"up"}}"#),
            Some(UiMode::Up)
        );
        assert_eq!(
            parse_ui_response(br#"{"v":1,"id":1,"ok":true,"result":{"mode":"classic"}}"#),
            Some(UiMode::Classic)
        );
        assert_eq!(
            parse_ui_response(
                b" { \"result\" : { \"x\" : [1, null, \"a\\\"b\"], \"mode\" : \"\\u0075p\" } ,\
                  \"ok\" : true , \"id\" : 1 , \"v\" : 1 }\r"
            ),
            Some(UiMode::Up)
        );
    }

    #[test]
    fn rejects_error_unknown_and_malformed_replies() {
        for reply in [
            &br#"{"v":1,"id":1,"ok":false,"error":{"code":"UNKNOWN_COMMAND","message":"x"}}"#[..],
            br#"{"v":1,"id":1,"ok":true,"result":{"mode":"sideways"}}"#,
            br#"{"v":1,"id":1,"ok":true,"result":{}}"#,
            br#"{"v":1,"id":1,"ok":true}"#,
            br#"{"v":1,"id":2,"ok":true,"result":{"mode":"up"}}"#,
            br#"{"v":1,"id":1,"ok":"true","result":{"mode":"up"}}"#,
            br#"{"v":1,"id":1,"ok":true,"result":{"mode":"up"}} trailing"#,
            br#"{"v":1,"id":1,"ok":true,"result":{"mode":"up"}"#,
            b"garbage",
            b"",
            b"[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[",
        ] {
            assert_eq!(
                parse_ui_response(reply),
                None,
                "{:?}",
                String::from_utf8_lossy(reply)
            );
        }
    }

    #[cfg(unix)]
    mod socket {
        use super::super::*;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::time::Instant;

        /// Fake window: accepts one connection, checks the request line, and
        /// replies with `reply` (or nothing, holding the connection open).
        fn fake_window(reply: Option<&'static [u8]>) -> (tempfile::TempDir, std::path::PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("nulix.sock");
            let listener = UnixListener::bind(&path).unwrap();
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = Vec::new();
                BufReader::new(&stream)
                    .read_until(b'\n', &mut line)
                    .unwrap();
                assert_eq!(line, UI_QUERY);
                match reply {
                    Some(reply) => {
                        let _ = stream.write_all(reply);
                    }
                    None => std::thread::sleep(Duration::from_secs(2)),
                }
            });
            (dir, path)
        }

        #[test]
        fn queries_up_and_classic_from_the_window() {
            let (_dir, path) = fake_window(Some(
                b"{\"v\":1,\"id\":1,\"ok\":true,\"result\":{\"mode\":\"up\"}}\n",
            ));
            assert_eq!(
                query_window_mode(&path, Duration::from_secs(5)),
                Some(UiMode::Up)
            );
            let (_dir, path) = fake_window(Some(
                b"{\"v\":1,\"id\":1,\"ok\":true,\"result\":{\"mode\":\"classic\"}}\n",
            ));
            assert_eq!(
                query_window_mode(&path, Duration::from_secs(5)),
                Some(UiMode::Classic)
            );
        }

        #[test]
        fn error_and_garbage_replies_yield_none() {
            let (_dir, path) = fake_window(Some(
                b"{\"v\":1,\"id\":1,\"ok\":false,\"error\":{\"code\":\"UNKNOWN_COMMAND\",\"message\":\"unknown command ui\"}}\n",
            ));
            assert_eq!(query_window_mode(&path, Duration::from_secs(5)), None);
            let (_dir, path) = fake_window(Some(b"\x00\xffnot json at all\n"));
            assert_eq!(query_window_mode(&path, Duration::from_secs(5)), None);
            let (_dir, path) = fake_window(Some(&[b'x'; 8192]));
            assert_eq!(query_window_mode(&path, Duration::from_secs(5)), None);
        }

        #[test]
        fn silent_window_times_out_within_the_budget() {
            let (_dir, path) = fake_window(None);
            let started = Instant::now();
            assert_eq!(query_window_mode(&path, Duration::from_millis(100)), None);
            assert!(
                started.elapsed() < Duration::from_millis(1000),
                "{:?}",
                started.elapsed()
            );
        }

        #[test]
        fn missing_socket_yields_none() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("missing.sock");
            assert_eq!(query_window_mode(&path, UI_QUERY_BUDGET), None);
        }
    }
}

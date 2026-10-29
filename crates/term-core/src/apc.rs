//! Bounded observer for opaque APC payloads in a PTY byte stream.

const DEFAULT_MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum State {
    #[default]
    Ground,
    Escape,
    Apc,
    ApcEscape,
    Discard,
    DiscardEscape,
}

/// Incrementally extracts complete 7-bit APC payloads (`ESC _ ... ESC \\`).
/// C1 controls are intentionally not recognized, matching the OSC observer.
pub struct Observer {
    state: State,
    payload: Vec<u8>,
    max_payload_bytes: usize,
}

impl Observer {
    pub fn new(max_payload_bytes: usize) -> Self {
        Self {
            state: State::Ground,
            payload: Vec::new(),
            max_payload_bytes,
        }
    }

    /// Return complete opaque payloads, without interpreting kitty command data.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut payloads = Vec::new();
        for &byte in bytes {
            match self.state {
                State::Ground => {
                    if byte == 0x1b {
                        self.state = State::Escape;
                    }
                }
                State::Escape => match byte {
                    b'_' => {
                        self.payload.clear();
                        self.state = State::Apc;
                    }
                    0x1b => self.state = State::Escape,
                    _ => self.state = State::Ground,
                },
                State::Apc => match byte {
                    0x18 | 0x1a => self.reset(),
                    0x1b => self.state = State::ApcEscape,
                    _ if self.payload.len() < self.max_payload_bytes => {
                        self.payload.push(byte);
                    }
                    _ => {
                        self.payload.clear();
                        self.state = State::Discard;
                    }
                },
                State::ApcEscape => match byte {
                    b'\\' => {
                        payloads.push(std::mem::take(&mut self.payload));
                        self.state = State::Ground;
                    }
                    0x18 | 0x1a => self.reset(),
                    0x1b => self.state = State::Escape,
                    b'_' => {
                        self.payload.clear();
                        self.state = State::Apc;
                    }
                    _ => self.state = State::Ground,
                },
                State::Discard => match byte {
                    0x18 | 0x1a => self.reset(),
                    0x1b => self.state = State::DiscardEscape,
                    _ => {}
                },
                State::DiscardEscape => match byte {
                    b'\\' | 0x18 | 0x1a => self.reset(),
                    0x1b => self.state = State::Escape,
                    b'_' => {
                        self.payload.clear();
                        self.state = State::Apc;
                    }
                    _ => self.state = State::Ground,
                },
            }
        }
        payloads
    }

    fn reset(&mut self) {
        self.payload.clear();
        self.state = State::Ground;
    }
}

impl Default for Observer {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_PAYLOAD_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(payload: &[u8]) -> Vec<u8> {
        [b"\x1b_".as_slice(), payload, b"\x1b\\"].concat()
    }

    #[test]
    fn yields_complete_payloads_and_handles_every_split() {
        let stream = framed(b"Ga=b,c");
        let expected = vec![b"Ga=b,c".to_vec()];
        let mut observer = Observer::new(32);
        assert_eq!(observer.feed(&stream), expected);
        for split in 0..=stream.len() {
            let mut observer = Observer::new(32);
            let mut actual = observer.feed(&stream[..split]);
            actual.extend(observer.feed(&stream[split..]));
            assert_eq!(actual, expected, "split at {split}");
        }
    }

    #[test]
    fn oversized_payload_is_discarded_through_st_then_resynchronizes() {
        let mut observer = Observer::new(3);
        let mut stream = framed(b"abcd");
        stream.extend(framed(b"Gx"));
        assert_eq!(observer.feed(&stream), vec![b"Gx".to_vec()]);
    }

    #[test]
    fn cancellation_and_non_st_escape_abort_without_partial_payload() {
        for cancel in [0x18, 0x1a] {
            let mut observer = Observer::new(32);
            let mut stream = b"\x1b_partial".to_vec();
            stream.push(cancel);
            stream.extend(framed(b"Gok"));
            assert_eq!(observer.feed(&stream), vec![b"Gok".to_vec()]);
        }
        let mut observer = Observer::new(32);
        assert!(observer.feed(b"\x1b_partial\x1bX").is_empty());
        assert_eq!(observer.feed(&framed(b"Gok")), vec![b"Gok".to_vec()]);

        let mut observer = Observer::new(32);
        assert_eq!(
            observer.feed(b"\x1b_partial\x1b\x1b_Gok\x1b\\"),
            vec![b"Gok".to_vec()]
        );

        let mut observer = Observer::new(32);
        assert_eq!(
            observer.feed(b"\x1b_partial\x1b_Gok\x1b\\"),
            vec![b"Gok".to_vec()]
        );
    }

    #[test]
    fn supports_back_to_back_interleaved_empty_and_opaque_payloads() {
        let mut observer = Observer::new(32);
        let mut stream = b"text\x1b]133;A\x07".to_vec();
        stream.extend(framed(b""));
        stream.extend(framed(&[0xff, 0x00, b'x']));
        assert_eq!(
            observer.feed(&stream),
            vec![Vec::new(), vec![0xff, 0x00, b'x']]
        );
    }
}

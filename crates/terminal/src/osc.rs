//! Incremental observation of shell-integration OSC sequences.

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SemanticMark {
    PromptStart,
    InputStart,
    OutputStart,
    CommandFinished { exit_status: Option<i32> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Event {
    SemanticMark(SemanticMark),
    /// OSC 7 reports a file URL. `path` is percent-decoded raw path bytes.
    CurrentDirectory {
        host: Vec<u8>,
        path: Vec<u8>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedEvent {
    /// Absolute stream offset immediately after the sequence terminator.
    pub end_offset: u64,
    pub event: Event,
}

const MAX_OSC_PAYLOAD_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ParseState {
    #[default]
    Ground,
    Escape,
    Osc,
    OscEscape,
    DiscardOsc,
    DiscardOscEscape,
}

pub struct Observer {
    state: ParseState,
    payload: Vec<u8>,
    stream_offset: u64,
}

impl Observer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe a chunk from the PTY and return complete recognized OSC events.
    ///
    /// Offsets count every input byte, including non-OSC output, and identify
    /// the exclusive end position immediately after each sequence terminator.
    /// The retained OSC payload never exceeds `MAX_OSC_PAYLOAD_BYTES`; an
    /// oversized sequence is discarded through its terminator before parsing
    /// resumes. The returned vector is proportional only to events completed
    /// in this chunk.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<ObservedEvent> {
        let mut events = Vec::new();

        for &byte in bytes {
            self.stream_offset = self.stream_offset.saturating_add(1);

            match self.state {
                ParseState::Ground => {
                    if byte == b'\x1b' {
                        self.state = ParseState::Escape;
                    }
                }
                ParseState::Escape => match byte {
                    b']' => {
                        self.payload.clear();
                        self.state = ParseState::Osc;
                    }
                    b'\x1b' => self.state = ParseState::Escape,
                    _ => self.state = ParseState::Ground,
                },
                ParseState::Osc => match byte {
                    b'\x07' => self.finish_osc(&mut events),
                    b'\x1b' => self.state = ParseState::OscEscape,
                    _ => {
                        self.append_payload(byte);
                    }
                },
                ParseState::OscEscape => match byte {
                    b'\\' => self.finish_osc(&mut events),
                    b'\x07' => {
                        if self.append_payload(b'\x1b') {
                            self.finish_osc(&mut events);
                        } else {
                            self.reset();
                        }
                    }
                    b'\x1b' => {
                        if self.append_payload(b'\x1b') {
                            self.state = ParseState::OscEscape;
                        } else {
                            // The current ESC may begin the discard terminator.
                            self.state = ParseState::DiscardOscEscape;
                        }
                    }
                    _ => {
                        if self.append_payload(b'\x1b') && self.append_payload(byte) {
                            self.state = ParseState::Osc;
                        }
                    }
                },
                ParseState::DiscardOsc => match byte {
                    b'\x07' => self.reset(),
                    b'\x1b' => self.state = ParseState::DiscardOscEscape,
                    _ => {}
                },
                ParseState::DiscardOscEscape => match byte {
                    b'\\' | b'\x07' => self.reset(),
                    b'\x1b' => self.state = ParseState::DiscardOscEscape,
                    _ => self.state = ParseState::DiscardOsc,
                },
            }
        }

        events
    }

    fn append_payload(&mut self, byte: u8) -> bool {
        if self.payload.len() == MAX_OSC_PAYLOAD_BYTES {
            self.payload.clear();
            self.state = ParseState::DiscardOsc;
            false
        } else {
            self.payload.push(byte);
            true
        }
    }

    fn finish_osc(&mut self, events: &mut Vec<ObservedEvent>) {
        if let Some(event) = parse_payload(&self.payload) {
            events.push(ObservedEvent {
                end_offset: self.stream_offset,
                event,
            });
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.payload.clear();
        self.state = ParseState::Ground;
    }
}

impl Default for Observer {
    fn default() -> Self {
        Self {
            state: ParseState::Ground,
            payload: Vec::with_capacity(MAX_OSC_PAYLOAD_BYTES),
            stream_offset: 0,
        }
    }
}

fn parse_payload(payload: &[u8]) -> Option<Event> {
    let (number, body) = split_once(payload, b';')?;
    match number {
        b"133" => parse_semantic_mark(body).map(Event::SemanticMark),
        b"7" => parse_file_url(body).map(|(host, path)| Event::CurrentDirectory { host, path }),
        _ => None,
    }
}

fn parse_semantic_mark(body: &[u8]) -> Option<SemanticMark> {
    let (subcommand, parameters) = match split_once(body, b';') {
        Some(parts) => parts,
        None => (body, &[][..]),
    };

    match subcommand {
        b"A" => Some(SemanticMark::PromptStart),
        b"B" => Some(SemanticMark::InputStart),
        b"C" => Some(SemanticMark::OutputStart),
        b"D" => {
            let status = match split_once(parameters, b';') {
                Some((status, _)) => status,
                None => parameters,
            };
            let exit_status = if status.is_empty() {
                None
            } else {
                Some(parse_decimal_status(status)?)
            };
            Some(SemanticMark::CommandFinished { exit_status })
        }
        _ => None,
    }
}

fn parse_decimal_status(bytes: &[u8]) -> Option<i32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }

    bytes.iter().try_fold(0_i32, |value, digit| {
        value.checked_mul(10)?.checked_add(i32::from(digit - b'0'))
    })
}

fn parse_file_url(url: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let remainder = url.strip_prefix(b"file://")?;
    let path_start = remainder.iter().position(|&byte| byte == b'/')?;
    let (host, encoded_path) = remainder.split_at(path_start);
    if encoded_path.is_empty() {
        return None;
    }

    Some((host.to_vec(), percent_decode(encoded_path)?))
}

fn percent_decode(path: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(path.len());
    let mut index = 0;
    while index < path.len() {
        if path[index] != b'%' {
            decoded.push(path[index]);
            index += 1;
            continue;
        }

        let high = *path.get(index + 1)?;
        let low = *path.get(index + 2)?;
        decoded.push((hex_value(high)? << 4) | hex_value(low)?);
        index += 3;
    }
    Some(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn split_once(bytes: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let index = bytes.iter().position(|&byte| byte == delimiter)?;
    Some((&bytes[..index], &bytes[index + 1..]))
}

#[cfg(test)]
mod tests {
    use super::{Event, MAX_OSC_PAYLOAD_BYTES, ObservedEvent, Observer, SemanticMark};

    fn sequence(payload: &[u8], terminator: &[u8]) -> Vec<u8> {
        [b"\x1b]".as_slice(), payload, terminator].concat()
    }

    fn expected(end_offset: usize, event: Event) -> ObservedEvent {
        ObservedEvent {
            end_offset: end_offset as u64,
            event,
        }
    }

    #[test]
    fn recognizes_each_semantic_mark_with_bel_and_st() {
        let cases = [
            (b"133;A".as_slice(), SemanticMark::PromptStart),
            (b"133;B".as_slice(), SemanticMark::InputStart),
            (b"133;C".as_slice(), SemanticMark::OutputStart),
            (
                b"133;D;1".as_slice(),
                SemanticMark::CommandFinished {
                    exit_status: Some(1),
                },
            ),
            (
                b"133;D".as_slice(),
                SemanticMark::CommandFinished { exit_status: None },
            ),
        ];
        let terminators = [b"\x07".as_slice(), b"\x1b\\".as_slice()];

        for (payload, mark) in cases {
            for terminator in terminators {
                let bytes = sequence(payload, terminator);
                let mut observer = Observer::new();
                assert_eq!(
                    observer.feed(&bytes),
                    vec![expected(bytes.len(), Event::SemanticMark(mark.clone()),)],
                    "payload={payload:?}, terminator={terminator:?}"
                );
            }
        }
    }

    #[test]
    fn decodes_file_url_path_bytes_with_both_terminators() {
        let terminators = [b"\x07".as_slice(), b"\x1b\\".as_slice()];
        for terminator in terminators {
            let bytes = sequence(b"7;file://build-host/a%20b/%FF", terminator);
            let mut observer = Observer::new();
            assert_eq!(
                observer.feed(&bytes),
                vec![expected(
                    bytes.len(),
                    Event::CurrentDirectory {
                        host: b"build-host".to_vec(),
                        path: b"/a b/\xff".to_vec(),
                    },
                )]
            );
        }
    }

    #[test]
    fn recognizes_real_so1sh_shell_integration_bytes() {
        // OSC bytes extracted from a PTY capture of `echo t2-capture` in /tmp
        // with SO1SH_SHELL_INTEGRATION=1 and a controlling terminal.
        let captured = b"\x1b]7;file://localhost/tmp\x1b\\\x1b]133;A\x1b\\\x1b]133;B\x1b\\\x1b]133;C\x1b\\\x1b]133;D;0\x1b\\";
        let mut observer = Observer::new();
        let events = observer.feed(captured);

        assert_eq!(events.len(), 5);
        assert_eq!(
            events
                .iter()
                .map(|event| event.event.clone())
                .collect::<Vec<_>>(),
            vec![
                Event::CurrentDirectory {
                    host: b"localhost".to_vec(),
                    path: b"/tmp".to_vec(),
                },
                Event::SemanticMark(SemanticMark::PromptStart),
                Event::SemanticMark(SemanticMark::InputStart),
                Event::SemanticMark(SemanticMark::OutputStart),
                Event::SemanticMark(SemanticMark::CommandFinished {
                    exit_status: Some(0),
                }),
            ]
        );
    }

    #[test]
    fn every_two_chunk_split_preserves_events_and_absolute_end_offsets() {
        let prompt = sequence(b"133;A", b"\x07");
        let input = sequence(b"133;B", b"\x1b\\");
        let output = sequence(b"133;C", b"\x07");
        let finish = sequence(b"133;D;1", b"\x1b\\");
        let cwd = sequence(b"7;file://host/tmp/a%20b", b"\x07");
        let stream = [
            prompt.as_slice(),
            input.as_slice(),
            output.as_slice(),
            b"output".as_slice(),
            finish.as_slice(),
            cwd.as_slice(),
        ]
        .concat();
        let prompt_end = prompt.len();
        let input_end = prompt_end + input.len();
        let output_end = input_end + output.len();
        let finish_end = output_end + b"output".len() + finish.len();
        let expected_events = vec![
            expected(prompt_end, Event::SemanticMark(SemanticMark::PromptStart)),
            expected(input_end, Event::SemanticMark(SemanticMark::InputStart)),
            expected(output_end, Event::SemanticMark(SemanticMark::OutputStart)),
            expected(
                finish_end,
                Event::SemanticMark(SemanticMark::CommandFinished {
                    exit_status: Some(1),
                }),
            ),
            expected(
                stream.len(),
                Event::CurrentDirectory {
                    host: b"host".to_vec(),
                    path: b"/tmp/a b".to_vec(),
                },
            ),
        ];

        for split in 0..=stream.len() {
            let mut observer = Observer::new();
            let mut actual = observer.feed(&stream[..split]);
            actual.extend(observer.feed(&stream[split..]));
            assert_eq!(actual, expected_events, "split at byte {split}");
        }
    }

    #[test]
    fn malformed_and_unknown_sequences_are_ignored_and_parser_resynchronizes() {
        let mut input = sequence(b"133;Z", b"\x07");
        input.extend(sequence(b"133;D;not-a-status", b"\x1b\\"));
        input.extend(sequence(b"7;file://host/no%Q0", b"\x07"));
        input.extend(sequence(b"7;https://host/path", b"\x1b\\"));
        input.extend(sequence(b"133;A", b"\x07"));

        let mut observer = Observer::new();
        assert_eq!(
            observer.feed(&input),
            vec![expected(
                input.len(),
                Event::SemanticMark(SemanticMark::PromptStart),
            )]
        );
    }

    #[test]
    fn oversized_osc_is_discarded_until_terminator_then_resynchronizes() {
        let mut oversized = b"\x1b]133;A;".to_vec();
        let remaining = MAX_OSC_PAYLOAD_BYTES - b"133;A;".len();
        oversized.extend(std::iter::repeat_n(b'x', remaining));
        let mut observer = Observer::new();
        assert!(observer.feed(&oversized).is_empty());
        assert_eq!(observer.payload.len(), MAX_OSC_PAYLOAD_BYTES);

        assert!(observer.feed(b"x").is_empty());
        assert!(observer.payload.is_empty());
        assert_eq!(observer.state, super::ParseState::DiscardOsc);

        // The nested-looking B mark remains part of the oversized payload;
        // the BEL terminator ends that discarded sequence and parsing resumes.
        assert!(observer.feed(b"\x1b]133;B\x07").is_empty());
        let valid = sequence(b"133;C", b"\x1b\\");
        assert_eq!(
            observer.feed(&valid),
            vec![expected(
                oversized.len() + 1 + b"\x1b]133;B\x07".len() + valid.len(),
                Event::SemanticMark(SemanticMark::OutputStart),
            )]
        );
    }
}

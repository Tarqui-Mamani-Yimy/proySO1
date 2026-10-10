use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key<'a> {
    Text(&'a str),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Right,
    Left,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Function(u8),
    Numpad(u8),
    NumpadEnter,
    NumpadKey(KeypadKey),
    Modifier(ModifierKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeypadKey {
    Decimal,
    Divide,
    Multiply,
    Subtract,
    Add,
    Equal,
    Separator,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Insert,
    Delete,
    Begin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModifierKey {
    LeftShift,
    RightShift,
    LeftControl,
    RightControl,
    LeftAlt,
    RightAlt,
    LeftSuper,
    RightSuper,
    CapsLock,
    NumLock,
}

/// Kitty keyboard protocol enhancement flags requested by the running program.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyboardFlags(u8);

impl KeyboardFlags {
    pub const DISAMBIGUATE: Self = Self(1);
    pub const REPORT_EVENT_TYPES: Self = Self(2);
    pub const REPORT_ALTERNATE: Self = Self(4);
    pub const REPORT_ALL: Self = Self(8);
    pub const REPORT_ASSOCIATED_TEXT: Self = Self(16);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl std::ops::BitOr for KeyboardFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyInput<'a> {
    pub key: Key<'a>,
    pub key_code: Option<u32>,
    pub shifted_key_code: Option<u32>,
    pub base_layout_key_code: Option<u32>,
    pub associated_text: Option<&'a str>,
}

impl<'a> KeyInput<'a> {
    pub const fn new(key: Key<'a>) -> Self {
        Self {
            key,
            key_code: None,
            shifted_key_code: None,
            base_layout_key_code: None,
            associated_text: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
    pub super_key: bool,
    pub hyper: bool,
    pub meta: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
}

#[derive(Default)]
pub struct ImePreeditGate {
    composing: bool,
}
impl ImePreeditGate {
    pub fn preedit(&mut self, text: &str) {
        self.composing = !text.is_empty();
    }
    pub fn commit(&mut self) {
        self.composing = false;
    }
    pub fn disable(&mut self) {
        self.composing = false;
    }
    pub fn suppresses_key(&self, key: Key<'_>, control: bool) -> bool {
        self.composing && !control && matches!(key, Key::Text(_))
    }
}

pub fn encode_key(key: Key<'_>, modifiers: Modifiers, application_cursor: bool) -> Option<Vec<u8>> {
    match key {
        Key::Text(value) if modifiers.control => {
            let bytes = value.bytes();
            let b = bytes.into_iter().next()?;
            let c = match b {
                // Xterm's conventional Ctrl aliases are letters plus the
                // explicit punctuation/digit pairs; other punctuation and
                // digits must not be folded into unrelated C0 controls.
                b' ' | b'@' | b'2' => 0,
                b'[' | b'3' => 0x1b,
                b'\\' | b'4' => 0x1c,
                b']' | b'5' => 0x1d,
                b'^' | b'6' => 0x1e,
                b'/' | b'_' | b'7' => 0x1f,
                b'?' | b'8' => 0x7f,
                b'A'..=b'Z' => b - b'A' + 1,
                b'a'..=b'z' => b - b'a' + 1,
                _ => return None,
            };
            return Some(vec![c]);
        }
        Key::Text(value) => {
            let mut out = Vec::new();
            if modifiers.alt {
                out.push(0x1b);
            }
            out.extend_from_slice(value.as_bytes());
            return Some(out);
        }
        Key::Enter => return Some(vec![b'\r']),
        Key::Backspace => return Some(vec![0x7f]),
        Key::Escape => return Some(vec![0x1b]),
        Key::Tab if modifiers.shift => return Some(b"\x1b[Z".to_vec()),
        Key::Tab => return Some(vec![b'\t']),
        _ => {}
    }
    let (final_char, app_capable) = match key {
        Key::Up => (b'A', true),
        Key::Down => (b'B', true),
        Key::Right => (b'C', true),
        Key::Left => (b'D', true),
        Key::Home => (b'H', true),
        Key::End => (b'F', true),
        _ => (0, false),
    };
    if app_capable {
        let modifier = modifier_parameter(modifiers);
        if modifier == 1 && application_cursor {
            return Some(vec![0x1b, b'O', final_char]);
        }
        let seq = if modifier == 1 {
            format!("\x1b[{}", final_char as char)
        } else {
            format!("\x1b[1;{modifier}{}", final_char as char)
        };
        return Some(seq.into_bytes());
    }
    let (code, terminator) = match key {
        Key::PageUp => (5, b'~'),
        Key::PageDown => (6, b'~'),
        Key::Insert => (2, b'~'),
        Key::Delete => (3, b'~'),
        Key::Function(n @ 1..=4) => {
            let final_char = b'P' + n - 1;
            let modifier = modifier_parameter(modifiers);
            if modifier == 1 {
                return Some(vec![0x1b, b'O', final_char]);
            }
            return Some(format!("\x1b[1;{modifier}{}", final_char as char).into_bytes());
        }
        Key::Function(n @ 5..=12) => (
            [0, 0, 0, 0, 15, 17, 18, 19, 20, 21, 23, 24][n as usize - 1],
            b'~',
        ),
        _ => return None,
    };
    let modifier = modifier_parameter(modifiers);
    let seq = if modifier == 1 {
        format!("\x1b[{code}{}", terminator as char)
    } else {
        format!("\x1b[{code};{modifier}{}", terminator as char)
    };
    Some(seq.into_bytes())
}

fn modifier_parameter(m: Modifiers) -> u8 {
    1 + u8::from(m.shift)
        + 2 * u8::from(m.alt)
        + 4 * u8::from(m.control)
        + 8 * u8::from(m.super_key)
}

/// Encode a key using the active kitty keyboard mode, falling back to the
/// existing xterm encoder byte-for-byte when no flags are active.
pub fn encode_key_with_flags(
    input: KeyInput<'_>,
    modifiers: Modifiers,
    application_cursor: bool,
    flags: KeyboardFlags,
    kind: KeyEventKind,
) -> Option<Vec<u8>> {
    if flags == KeyboardFlags::empty() {
        return (kind != KeyEventKind::Release)
            .then(|| encode_key(input.key, modifiers, application_cursor))
            .flatten();
    }

    let all = flags.contains(KeyboardFlags::REPORT_ALL);
    let disambiguate = flags.contains(KeyboardFlags::DISAMBIGUATE);
    let event_types = flags.contains(KeyboardFlags::REPORT_EVENT_TYPES);
    let base_code = input.key_code.or_else(|| kitty_key_code(input.key))?;
    let is_text = matches!(input.key, Key::Text(_));
    let legacy_control_key = matches!(input.key, Key::Enter | Key::Tab | Key::Backspace)
        && !(matches!(input.key, Key::Tab) && modifiers.shift);
    let needs_sequence = all
        || (!is_text && !legacy_control_key && (disambiguate || event_types))
        || (is_text && disambiguate && (modifiers.alt || modifiers.control || modifiers.super_key));

    if !needs_sequence {
        if kind == KeyEventKind::Release {
            return None;
        }
        return encode_key(input.key, modifiers, application_cursor);
    }
    if kind == KeyEventKind::Release && !event_types {
        return None;
    }
    if !all
        && !event_types
        && kitty_modifier_parameter(modifiers) == 1
        && !matches!(input.key, Key::Escape)
        && let Some(bytes) = encode_key(input.key, modifiers, application_cursor)
    {
        return Some(bytes);
    }
    if let Some(bytes) = encode_kitty_functional_key(input.key, modifiers, kind, event_types) {
        return Some(bytes);
    }

    let alternate = flags.contains(KeyboardFlags::REPORT_ALTERNATE);
    let mut code = base_code.to_string();
    if alternate {
        if modifiers.shift
            && let Some(shifted) = input.shifted_key_code
        {
            code.push(':');
            code.push_str(&shifted.to_string());
        }
        if let Some(base) = input.base_layout_key_code {
            if !code.contains(':') {
                code.push(':');
            }
            code.push(':');
            code.push_str(&base.to_string());
        }
    }

    let modifier = kitty_modifier_parameter(modifiers);
    let mut modifier_field = modifier.to_string();
    if event_types {
        let event_type = match kind {
            KeyEventKind::Press => 1,
            KeyEventKind::Repeat => 2,
            KeyEventKind::Release => 3,
        };
        modifier_field.push(':');
        modifier_field.push_str(&event_type.to_string());
    }

    let associated = if all && flags.contains(KeyboardFlags::REPORT_ASSOCIATED_TEXT) {
        input
            .associated_text
            .map(safe_text_codepoints)
            .filter(|text| !text.is_empty())
    } else {
        None
    };
    let include_modifier = modifier != 1 || event_types || associated.is_some();
    let mut sequence = format!("\x1b[{code}");
    if include_modifier {
        sequence.push(';');
        sequence.push_str(&modifier_field);
    }
    if let Some(text) = associated {
        sequence.push(';');
        sequence.push_str(&text);
    }
    sequence.push('u');
    Some(sequence.into_bytes())
}

fn kitty_modifier_parameter(modifiers: Modifiers) -> u16 {
    1 + u16::from(modifiers.shift)
        + 2 * u16::from(modifiers.alt)
        + 4 * u16::from(modifiers.control)
        + 8 * u16::from(modifiers.super_key)
        + 16 * u16::from(modifiers.hyper)
        + 32 * u16::from(modifiers.meta)
        + 64 * u16::from(modifiers.caps_lock)
        + 128 * u16::from(modifiers.num_lock)
}

fn safe_text_codepoints(text: &str) -> String {
    text.chars()
        .filter(|character| {
            *character as u32 >= 0x20 && !(0x7f..=0x9f).contains(&(*character as u32))
        })
        .map(|character| character as u32)
        .map(|codepoint| codepoint.to_string())
        .collect::<Vec<_>>()
        .join(":")
}

fn encode_kitty_functional_key(
    key: Key<'_>,
    modifiers: Modifiers,
    kind: KeyEventKind,
    event_types: bool,
) -> Option<Vec<u8>> {
    let (parameter, final_byte) = match key {
        Key::Up => (1, b'A'),
        Key::Down => (1, b'B'),
        Key::Right => (1, b'C'),
        Key::Left => (1, b'D'),
        Key::Home => (1, b'H'),
        Key::End => (1, b'F'),
        Key::Function(1) => (1, b'P'),
        Key::Function(2) => (1, b'Q'),
        Key::Function(4) => (1, b'S'),
        Key::PageUp => (5, b'~'),
        Key::PageDown => (6, b'~'),
        Key::Insert => (2, b'~'),
        Key::Delete => (3, b'~'),
        Key::Function(3) => (13, b'~'),
        Key::Function(5) => (15, b'~'),
        Key::Function(6) => (17, b'~'),
        Key::Function(7) => (18, b'~'),
        Key::Function(8) => (19, b'~'),
        Key::Function(9) => (20, b'~'),
        Key::Function(10) => (21, b'~'),
        Key::Function(11) => (23, b'~'),
        Key::Function(12) => (24, b'~'),
        _ => return None,
    };
    let modifier = kitty_modifier_parameter(modifiers);
    let include_modifier = modifier != 1 || event_types;
    let mut sequence = format!("\x1b[{parameter}");
    if include_modifier {
        sequence.push(';');
        sequence.push_str(&modifier.to_string());
        if event_types {
            let event_type = match kind {
                KeyEventKind::Press => 1,
                KeyEventKind::Repeat => 2,
                KeyEventKind::Release => 3,
            };
            sequence.push(':');
            sequence.push_str(&event_type.to_string());
        }
    }
    sequence.push(final_byte as char);
    Some(sequence.into_bytes())
}

fn kitty_key_code(key: Key<'_>) -> Option<u32> {
    Some(match key {
        Key::Text(text) => text.chars().next()? as u32,
        Key::Enter => 13,
        Key::Tab => 9,
        Key::Backspace => 127,
        Key::Escape => 27,
        Key::Up => 57_352,
        Key::Down => 57_353,
        Key::Left => 57_350,
        Key::Right => 57_351,
        Key::Home => 57_356,
        Key::End => 57_357,
        Key::PageUp => 57_354,
        Key::PageDown => 57_355,
        Key::Insert => 57_348,
        Key::Delete => 57_349,
        Key::Function(number @ 1..=35) => 57_363 + u32::from(number),
        Key::Numpad(number @ 0..=9) => 57_399 + u32::from(number),
        Key::NumpadEnter => 57_414,
        Key::NumpadKey(key) => match key {
            KeypadKey::Decimal => 57_409,
            KeypadKey::Divide => 57_410,
            KeypadKey::Multiply => 57_411,
            KeypadKey::Subtract => 57_412,
            KeypadKey::Add => 57_413,
            KeypadKey::Equal => 57_415,
            KeypadKey::Separator => 57_416,
            KeypadKey::Left => 57_417,
            KeypadKey::Right => 57_418,
            KeypadKey::Up => 57_419,
            KeypadKey::Down => 57_420,
            KeypadKey::PageUp => 57_421,
            KeypadKey::PageDown => 57_422,
            KeypadKey::Home => 57_423,
            KeypadKey::End => 57_424,
            KeypadKey::Insert => 57_425,
            KeypadKey::Delete => 57_426,
            KeypadKey::Begin => 57_427,
        },
        Key::Modifier(modifier) => match modifier {
            ModifierKey::LeftShift => 57_441,
            ModifierKey::LeftControl => 57_442,
            ModifierKey::LeftAlt => 57_443,
            ModifierKey::LeftSuper => 57_444,
            ModifierKey::RightShift => 57_447,
            ModifierKey::RightControl => 57_448,
            ModifierKey::RightAlt => 57_449,
            ModifierKey::RightSuper => 57_450,
            ModifierKey::CapsLock => 57_358,
            ModifierKey::NumLock => 57_360,
        },
        _ => return None,
    })
}

pub fn encode_ime_commit(text: &str, _bracketed_paste: bool) -> Vec<u8> {
    text.as_bytes().to_vec()
}

pub fn encode_ime_commit_with_flags(text: &str, flags: KeyboardFlags) -> Vec<u8> {
    if !flags.contains(KeyboardFlags::REPORT_ALL) {
        return text.as_bytes().to_vec();
    }
    let mut sequence = String::from("\x1b[0");
    let text = safe_text_codepoints(text);
    if flags.contains(KeyboardFlags::REPORT_ASSOCIATED_TEXT) && !text.is_empty() {
        sequence.push_str(";1;");
        sequence.push_str(&text);
    }
    sequence.push('u');
    sequence.into_bytes()
}

pub fn encode_paste(text: &str, bracketed: bool) -> Cow<'_, [u8]> {
    let sanitized = sanitize_paste(text);
    let bytes = if sanitized == text {
        Cow::Borrowed(text.as_bytes())
    } else {
        Cow::Owned(sanitized.into_bytes())
    };
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(&bytes);
        out.extend_from_slice(b"\x1b[201~");
        Cow::Owned(out)
    } else {
        bytes
    }
}

pub fn sanitize_paste(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(*character, '\n' | '\r' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_keyboard_flags_encode_protocol_modes_and_events() {
        let cases = [
            (
                KeyInput::new(Key::Escape),
                Modifiers::default(),
                KeyboardFlags::DISAMBIGUATE,
                KeyEventKind::Press,
                b"\x1b[27u".as_slice(),
            ),
            (
                KeyInput::new(Key::Text("a")),
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                KeyboardFlags::DISAMBIGUATE,
                KeyEventKind::Press,
                b"\x1b[97;3u".as_slice(),
            ),
            (
                KeyInput::new(Key::Text("a")),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                KeyboardFlags::DISAMBIGUATE,
                KeyEventKind::Press,
                b"\x1b[97;5u".as_slice(),
            ),
            (
                KeyInput::new(Key::Tab),
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
                KeyboardFlags::DISAMBIGUATE,
                KeyEventKind::Press,
                b"\x1b[9;2u".as_slice(),
            ),
            (
                KeyInput::new(Key::Enter),
                Modifiers::default(),
                KeyboardFlags::DISAMBIGUATE,
                KeyEventKind::Press,
                b"\r".as_slice(),
            ),
            (
                KeyInput::new(Key::Enter),
                Modifiers::default(),
                KeyboardFlags::REPORT_ALL,
                KeyEventKind::Press,
                b"\x1b[13u".as_slice(),
            ),
            (
                KeyInput::new(Key::Text("a")),
                Modifiers::default(),
                KeyboardFlags::REPORT_ALL,
                KeyEventKind::Press,
                b"\x1b[97u".as_slice(),
            ),
            (
                KeyInput::new(Key::Up),
                Modifiers::default(),
                KeyboardFlags::REPORT_EVENT_TYPES,
                KeyEventKind::Press,
                b"\x1b[1;1:1A".as_slice(),
            ),
            (
                KeyInput::new(Key::Up),
                Modifiers::default(),
                KeyboardFlags::REPORT_EVENT_TYPES,
                KeyEventKind::Repeat,
                b"\x1b[1;1:2A".as_slice(),
            ),
            (
                KeyInput::new(Key::Up),
                Modifiers::default(),
                KeyboardFlags::REPORT_EVENT_TYPES,
                KeyEventKind::Release,
                b"\x1b[1;1:3A".as_slice(),
            ),
            (
                KeyInput::new(Key::PageUp),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                KeyboardFlags::DISAMBIGUATE,
                KeyEventKind::Press,
                b"\x1b[5;5~".as_slice(),
            ),
            (
                KeyInput::new(Key::Numpad(4)),
                Modifiers::default(),
                KeyboardFlags::DISAMBIGUATE,
                KeyEventKind::Press,
                b"\x1b[57403u".as_slice(),
            ),
            (
                KeyInput::new(Key::NumpadEnter),
                Modifiers::default(),
                KeyboardFlags::DISAMBIGUATE,
                KeyEventKind::Press,
                b"\x1b[57414u".as_slice(),
            ),
            (
                KeyInput::new(Key::NumpadKey(KeypadKey::Add)),
                Modifiers::default(),
                KeyboardFlags::DISAMBIGUATE,
                KeyEventKind::Press,
                b"\x1b[57413u".as_slice(),
            ),
            (
                KeyInput::new(Key::Function(1)),
                Modifiers::default(),
                KeyboardFlags::REPORT_ALL,
                KeyEventKind::Press,
                b"\x1b[1P".as_slice(),
            ),
            (
                KeyInput::new(Key::Up),
                Modifiers::default(),
                KeyboardFlags::DISAMBIGUATE,
                KeyEventKind::Press,
                b"\x1b[A".as_slice(),
            ),
        ];
        for (input, modifiers, flags, kind, expected) in cases {
            assert_eq!(
                encode_key_with_flags(input, modifiers, false, flags, kind).as_deref(),
                Some(expected)
            );
        }
        assert_eq!(
            encode_key_with_flags(
                KeyInput::new(Key::Up),
                Modifiers::default(),
                true,
                KeyboardFlags::DISAMBIGUATE,
                KeyEventKind::Press,
            )
            .unwrap(),
            b"\x1bOA"
        );
    }

    #[test]
    fn kitty_alternate_keys_and_associated_text_use_protocol_fields() {
        let mut input = KeyInput::new(Key::Text("a"));
        input.shifted_key_code = Some(65);
        input.base_layout_key_code = Some(97);
        assert_eq!(
            encode_key_with_flags(
                input,
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
                false,
                KeyboardFlags::REPORT_ALL | KeyboardFlags::REPORT_ALTERNATE,
                KeyEventKind::Press
            )
            .unwrap(),
            b"\x1b[97:65:97;2u"
        );
        input.associated_text = Some("A");
        assert_eq!(
            encode_key_with_flags(
                input,
                Modifiers::default(),
                false,
                KeyboardFlags::REPORT_ALL | KeyboardFlags::REPORT_ASSOCIATED_TEXT,
                KeyEventKind::Press
            )
            .unwrap(),
            b"\x1b[97;1;65u"
        );
    }

    #[test]
    fn kitty_modifier_bits_include_lock_hyper_and_meta() {
        let modifiers = Modifiers {
            shift: true,
            alt: true,
            control: true,
            super_key: true,
            hyper: true,
            meta: true,
            caps_lock: true,
            num_lock: true,
        };
        assert_eq!(
            encode_key_with_flags(
                KeyInput::new(Key::Text("x")),
                modifiers,
                false,
                KeyboardFlags::REPORT_ALL,
                KeyEventKind::Press,
            )
            .unwrap(),
            b"\x1b[120;256u"
        );
    }

    #[test]
    fn kitty_associated_text_omits_control_codepoints() {
        assert_eq!(
            encode_ime_commit_with_flags(
                "a\x1b\u{009b}λ",
                KeyboardFlags::REPORT_ALL | KeyboardFlags::REPORT_ASSOCIATED_TEXT
            ),
            b"\x1b[0;1;97:955u"
        );
    }

    #[test]
    fn kitty_flags_zero_preserve_legacy_encoding_and_release_is_ignored() {
        for key in [
            Key::Text("λ"),
            Key::Escape,
            Key::Up,
            Key::Tab,
            Key::Function(5),
        ] {
            assert_eq!(
                encode_key_with_flags(
                    KeyInput::new(key),
                    Modifiers::default(),
                    true,
                    KeyboardFlags::empty(),
                    KeyEventKind::Press
                ),
                encode_key(key, Modifiers::default(), true),
            );
        }
        assert_eq!(
            encode_key_with_flags(
                KeyInput::new(Key::Up),
                Modifiers::default(),
                false,
                KeyboardFlags::empty(),
                KeyEventKind::Release
            ),
            None
        );
    }

    #[test]
    fn event_type_reporting_keeps_text_as_text_unless_all_keys_is_enabled() {
        let input = KeyInput::new(Key::Text("x"));
        let flags = KeyboardFlags::REPORT_EVENT_TYPES;
        assert_eq!(
            encode_key_with_flags(
                input,
                Modifiers::default(),
                false,
                flags,
                KeyEventKind::Press
            ),
            Some(b"x".to_vec())
        );
        assert_eq!(
            encode_key_with_flags(
                input,
                Modifiers::default(),
                false,
                flags,
                KeyEventKind::Repeat
            ),
            Some(b"x".to_vec())
        );
        assert_eq!(
            encode_key_with_flags(
                input,
                Modifiers::default(),
                false,
                flags,
                KeyEventKind::Release
            ),
            None
        );
    }

    #[test]
    fn encodes_basic_text_and_control_keys() {
        let cases = [
            (
                Key::Text("λ"),
                Modifiers::default(),
                false,
                "λ".as_bytes().to_vec(),
            ),
            (Key::Enter, Modifiers::default(), false, b"\r".to_vec()),
            (Key::Tab, Modifiers::default(), false, b"\t".to_vec()),
            (
                Key::Tab,
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
                false,
                b"\x1b[Z".to_vec(),
            ),
            (
                Key::Backspace,
                Modifiers::default(),
                false,
                b"\x7f".to_vec(),
            ),
            (Key::Escape, Modifiers::default(), false, b"\x1b".to_vec()),
            (
                Key::Text("c"),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                false,
                b"\x03".to_vec(),
            ),
            (
                Key::Text("d"),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                false,
                b"\x04".to_vec(),
            ),
            (
                Key::Text("z"),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                false,
                b"\x1a".to_vec(),
            ),
            (
                Key::Text("\\"),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                false,
                b"\x1c".to_vec(),
            ),
            (
                Key::Text("r"),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                false,
                b"\x12".to_vec(),
            ),
            (
                Key::Text("l"),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                false,
                b"\x0c".to_vec(),
            ),
            (
                Key::Text("x"),
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                false,
                b"\x1bx".to_vec(),
            ),
        ];
        for (key, modifiers, app, expected) in cases {
            assert_eq!(encode_key(key, modifiers, app).unwrap(), expected);
        }
    }

    #[test]
    fn control_punctuation_and_digits_follow_xterm_c0_conventions() {
        let cases = [
            (" ", 0),
            ("@", 0),
            ("2", 0),
            ("3", 0x1b),
            ("4", 0x1c),
            ("5", 0x1d),
            ("6", 0x1e),
            ("7", 0x1f),
            ("8", 0x7f),
            ("[", 0x1b),
            ("\\", 0x1c),
            ("]", 0x1d),
            ("^", 0x1e),
            ("_", 0x1f),
            ("?", 0x7f),
        ];
        for (text, expected) in cases {
            assert_eq!(
                encode_key(
                    Key::Text(text),
                    Modifiers {
                        control: true,
                        ..Default::default()
                    },
                    false
                ),
                Some(vec![expected]),
                "Ctrl-{text:?}"
            );
        }
        assert_eq!(
            encode_key(
                Key::Text("/"),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                false,
            ),
            Some(vec![0x1f]),
            "Ctrl+/ is the xterm alias for Ctrl+_"
        );
    }

    #[test]
    fn encodes_navigation_and_function_keys_in_normal_and_application_cursor_modes() {
        let cases = [
            (Key::Up, b"\x1b[A".as_slice(), b"\x1bOA".as_slice()),
            (Key::Down, b"\x1b[B", b"\x1bOB"),
            (Key::Right, b"\x1b[C", b"\x1bOC"),
            (Key::Left, b"\x1b[D", b"\x1bOD"),
            (Key::Home, b"\x1b[H", b"\x1bOH"),
            (Key::End, b"\x1b[F", b"\x1bOF"),
        ];
        for (key, normal, app) in cases {
            assert_eq!(
                encode_key(key, Modifiers::default(), false).unwrap(),
                normal
            );
            assert_eq!(encode_key(key, Modifiers::default(), true).unwrap(), app);
        }
        let cases = [
            (Key::PageUp, b"\x1b[5~".as_slice()),
            (Key::PageDown, b"\x1b[6~"),
            (Key::Insert, b"\x1b[2~"),
            (Key::Delete, b"\x1b[3~"),
            (Key::Function(1), b"\x1bOP"),
            (Key::Function(2), b"\x1bOQ"),
            (Key::Function(3), b"\x1bOR"),
            (Key::Function(4), b"\x1bOS"),
            (Key::Function(5), b"\x1b[15~"),
            (Key::Function(6), b"\x1b[17~"),
            (Key::Function(7), b"\x1b[18~"),
            (Key::Function(8), b"\x1b[19~"),
            (Key::Function(9), b"\x1b[20~"),
            (Key::Function(10), b"\x1b[21~"),
            (Key::Function(11), b"\x1b[23~"),
            (Key::Function(12), b"\x1b[24~"),
        ];
        for (key, expected) in cases {
            assert_eq!(
                encode_key(key, Modifiers::default(), true).unwrap(),
                expected
            );
            assert_eq!(
                encode_key(key, Modifiers::default(), false).unwrap(),
                expected
            );
        }
        for text in ["!", "1", "0", "-"] {
            assert_eq!(
                encode_key(
                    Key::Text(text),
                    Modifiers {
                        control: true,
                        ..Default::default()
                    },
                    false,
                ),
                None,
                "Ctrl+{text} is not an xterm C0 alias"
            );
        }
    }

    #[test]
    fn control_text_encodes_xterm_ascii_control_variants() {
        let cases = [
            (" ", 0),
            ("@", 0),
            ("2", 0),
            ("?", 0x7f),
            ("8", 0x7f),
            ("[", 0x1b),
            ("3", 0x1b),
            ("\\", 0x1c),
            ("4", 0x1c),
            ("]", 0x1d),
            ("5", 0x1d),
            ("^", 0x1e),
            ("6", 0x1e),
            ("_", 0x1f),
            ("7", 0x1f),
        ];
        for (text, expected) in cases {
            assert_eq!(
                encode_key(
                    Key::Text(text),
                    Modifiers {
                        control: true,
                        ..Default::default()
                    },
                    false
                )
                .unwrap(),
                [expected]
            );
        }
    }

    #[test]
    fn super_modifier_does_not_prefix_printable_text() {
        assert_eq!(
            encode_key(
                Key::Text("x"),
                Modifiers {
                    super_key: true,
                    ..Default::default()
                },
                false
            )
            .unwrap(),
            b"x"
        );
    }

    fn composing_gate() -> ImePreeditGate {
        ImePreeditGate { composing: true }
    }
    #[test]
    fn cancelled_composition_reenables_keyboard_text() {
        let mut state = composing_gate();
        state.preedit("");
        assert!(!state.suppresses_key(Key::Text("x"), false));
    }

    #[test]
    fn non_text_keys_pass_during_composition() {
        use Key::*;
        let state = composing_gate();
        for key in [Enter, Tab, Backspace, Escape, Up, Home, Function(1)] {
            assert!(!state.suppresses_key(key, false));
        }
        assert!(!state.suppresses_key(Text("c"), true));
    }

    #[test]
    fn text_keys_stay_suppressed_during_active_preedit() {
        let state = composing_gate();
        assert!(state.suppresses_key(Key::Text("x"), false));
    }

    #[test]
    fn modified_navigation_uses_xterm_modifier_parameter() {
        assert_eq!(
            encode_key(
                Key::Right,
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                false
            )
            .unwrap(),
            b"\x1b[1;5C"
        );
        assert_eq!(
            encode_key(
                Key::Left,
                Modifiers {
                    shift: true,
                    alt: true,
                    ..Default::default()
                },
                false
            )
            .unwrap(),
            b"\x1b[1;4D"
        );
    }

    #[test]
    fn function_keys_apply_xterm_modifier_parameters() {
        let cases = [
            (
                Key::Function(1),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                b"\x1b[1;5P".as_slice(),
            ),
            (
                Key::Function(2),
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
                b"\x1b[1;2Q",
            ),
            (
                Key::Function(3),
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                b"\x1b[1;3R",
            ),
            (
                Key::Function(4),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                b"\x1b[1;5S",
            ),
        ];
        for (key, modifiers, expected) in cases {
            assert_eq!(encode_key(key, modifiers, false).unwrap(), expected);
        }
    }

    #[test]
    fn ime_commit_is_typed_text_not_bracketed_paste() {
        assert_eq!(encode_ime_commit("λ", true), "λ".as_bytes());
    }

    #[test]
    fn bracketed_paste_wraps_only_when_mode_is_enabled() {
        assert_eq!(encode_paste("aλ", false), "aλ".as_bytes());
        assert_eq!(
            encode_paste("aλ", true).as_ref(),
            b"\x1b[200~a\xce\xbb\x1b[201~"
        );
    }

    #[test]
    fn paste_sanitizes_escape_and_controls_before_bracketing() {
        assert_eq!(sanitize_paste("a\x1b[201~\u{009b}b\n\t"), "a[201~b\n\t");
        assert_eq!(
            encode_paste("a\x1b[201~", true).as_ref(),
            b"\x1b[200~a[201~\x1b[201~"
        );
        assert_eq!(encode_paste("a\x1b[201~", false).as_ref(), b"a[201~");
    }
}

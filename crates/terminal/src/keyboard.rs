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
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
    pub super_key: bool,
}

pub fn encode_key(key: Key<'_>, modifiers: Modifiers, application_cursor: bool) -> Option<Vec<u8>> {
    match key {
        Key::Text(value) if modifiers.control => {
            let bytes = value.bytes();
            let b = bytes.into_iter().next()?;
            let c = match b {
                b'@'..=b'_' => b & 0x1f,
                b'a'..=b'z' => b - b'a' + 1,
                _ => return None,
            };
            return Some(vec![c]);
        }
        Key::Text(value) => {
            let mut out = Vec::new();
            if modifiers.alt || modifiers.super_key {
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

pub fn encode_ime_commit(text: &str, _bracketed_paste: bool) -> Vec<u8> {
    text.as_bytes().to_vec()
}

pub fn encode_paste(text: &str, bracketed: bool) -> Cow<'_, [u8]> {
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

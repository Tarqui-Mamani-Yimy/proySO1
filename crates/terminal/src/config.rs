//! Startup configuration file for nulix and nulix-server.
//!
//! The file is plain `key = value` lines (no dependency, same spirit as the
//! shellux policy file). Blank lines and lines whose first non-blank character
//! is `#` are ignored; colour values start with `#`, so comments are only
//! recognised at the start of a line. It is looked up at `$NULIX_CONFIG`
//! when non-empty, else `$XDG_CONFIG_HOME/nulix/nulix.conf`, else
//! `$HOME/.config/nulix/nulix.conf`. A missing default file is silent.
//!
//! Every problem is reported as `nulix: config FILE:LINE: MESSAGE` and the
//! affected key keeps its default, so a bad file never stops the window from
//! opening. `NULIX_FONT`, `NULIX_FONT_SIZE` and `NULIX_SHELL` override
//! the matching keys where they are consumed. Hot reload is not implemented.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::model::DEFAULT_SCROLLBACK;
use crate::renderer::Palette;

/// Largest accepted `scrollback`, in lines.
pub const MAX_SCROLLBACK: usize = 100_000;
/// Accepted `font_size` range, in points.
pub const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 6.0..=72.0;

/// Settings read from the configuration file; unset keys keep their defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Extra font file loaded at startup (`font`).
    pub font: Option<PathBuf>,
    /// Font size in points (`font_size`).
    pub font_size: Option<f32>,
    /// Shell program for new panes (`shell`).
    pub shell: Option<OsString>,
    /// History lines kept per pane (`scrollback`).
    pub scrollback: usize,
    /// Colours (`foreground`, `background`, `cursor`, `color0`..`color15`).
    pub palette: Palette,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font: None,
            font_size: None,
            shell: None,
            scrollback: DEFAULT_SCROLLBACK,
            palette: Palette::default(),
        }
    }
}

/// Where the configuration file is, and whether the user named it explicitly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigLocation {
    pub path: PathBuf,
    pub explicit: bool,
}

/// Resolves the configuration path from environment lookups.
pub fn config_location(var: impl Fn(&str) -> Option<OsString>) -> Option<ConfigLocation> {
    let non_empty = |name: &str| var(name).filter(|value| !value.is_empty());
    if let Some(path) = non_empty("NULIX_CONFIG") {
        return Some(ConfigLocation {
            path: PathBuf::from(path),
            explicit: true,
        });
    }
    let base = non_empty("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| non_empty("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(ConfigLocation {
        path: base.join("nulix").join("nulix.conf"),
        explicit: false,
    })
}

/// Parses configuration text. Diagnostics are complete `nulix: config ...`
/// lines; the returned config holds every valid key.
pub fn parse_config(text: &str, origin: &Path, home: Option<&Path>) -> (Config, Vec<String>) {
    let mut config = Config::default();
    let mut diagnostics = Vec::new();
    // Some editors save a UTF-8 byte order mark; it is not part of the first key.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let report = |message: String| {
            format!(
                "nulix: config {}:{}: {message}",
                origin.display(),
                index + 1
            )
        };
        let Some((key, value)) = line.split_once('=') else {
            diagnostics.push(report("expected KEY = VALUE".into()));
            continue;
        };
        if let Err(message) = apply_setting(&mut config, key.trim(), value.trim(), home) {
            diagnostics.push(report(message));
        }
    }
    (config, diagnostics)
}

fn apply_setting(
    config: &mut Config,
    key: &str,
    value: &str,
    home: Option<&Path>,
) -> Result<(), String> {
    match key {
        "font" => config.font = Some(PathBuf::from(non_empty_path(key, value, home)?)),
        "font_size" => {
            let size = value
                .parse::<f32>()
                .ok()
                .filter(|size| size.is_finite() && FONT_SIZE_RANGE.contains(size))
                .ok_or_else(|| {
                    format!("font_size: expected a number from 6 to 72, got {value:?}")
                })?;
            config.font_size = Some(size);
        }
        "shell" => config.shell = Some(non_empty_path(key, value, home)?),
        "scrollback" => {
            config.scrollback = value
                .parse::<usize>()
                .ok()
                .filter(|lines| *lines <= MAX_SCROLLBACK)
                .ok_or_else(|| {
                    format!("scrollback: expected 0 to {MAX_SCROLLBACK} lines, got {value:?}")
                })?;
        }
        "foreground" => config.palette.foreground = parse_color(key, value)?,
        "background" => config.palette.background = parse_color(key, value)?,
        "cursor" => config.palette.cursor = Some(parse_color(key, value)?),
        _ => {
            let slot = key
                .strip_prefix("color")
                // Exactly `color0`..`color15`: no sign, no leading zero.
                .filter(|digits| {
                    !digits.is_empty()
                        && digits.bytes().all(|b| b.is_ascii_digit())
                        && (digits.len() == 1 || !digits.starts_with('0'))
                })
                .and_then(|digits| digits.parse::<usize>().ok())
                .filter(|slot| *slot < 16)
                .ok_or_else(|| format!("unknown key: {key}"))?;
            config.palette.ansi[slot] = parse_color(key, value)?;
        }
    }
    Ok(())
}

/// A path value; a leading `~/` is relative to HOME.
fn non_empty_path(key: &str, value: &str, home: Option<&Path>) -> Result<OsString, String> {
    if value.is_empty() {
        return Err(format!("{key}: empty value"));
    }
    match (value.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => Ok(home.join(rest).into_os_string()),
        _ => Ok(OsString::from(value)),
    }
}

fn parse_color(key: &str, value: &str) -> Result<[u8; 3], String> {
    let invalid = || format!("{key}: expected #rrggbb, got {value:?}");
    let hex = value.strip_prefix('#').ok_or_else(invalid)?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| invalid());
    Ok([channel(0)?, channel(2)?, channel(4)?])
}

/// Reads the configuration file named by the environment. A missing default
/// file yields the defaults silently; any other problem is a diagnostic.
pub fn load_config(var: impl Fn(&str) -> Option<OsString>) -> (Config, Vec<String>) {
    let home = var("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let Some(location) = config_location(&var) else {
        return (Config::default(), Vec::new());
    };
    match std::fs::read(&location.path) {
        Ok(bytes) => parse_config(
            &String::from_utf8_lossy(&bytes),
            &location.path,
            home.as_deref(),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !location.explicit => {
            (Config::default(), Vec::new())
        }
        Err(error) => (
            Config::default(),
            vec![format!(
                "nulix: config {}: {error}",
                location.path.display()
            )],
        ),
    }
}

static CURRENT: OnceLock<Config> = OnceLock::new();

/// Loads the process configuration once, printing diagnostics to stderr.
/// Later calls return the same configuration.
pub fn init() -> &'static Config {
    CURRENT.get_or_init(|| {
        let (config, diagnostics) = load_config(|name| std::env::var_os(name));
        for line in diagnostics {
            eprintln!("{line}");
        }
        config
    })
}

/// The process configuration: what [`init`] loaded, or the defaults.
pub fn current() -> &'static Config {
    CURRENT.get().unwrap_or_else(|| {
        static DEFAULT: OnceLock<Config> = OnceLock::new();
        DEFAULT.get_or_init(Config::default)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn parse(text: &str) -> (Config, Vec<String>) {
        parse_config(
            text,
            Path::new("/cfg/nulix.conf"),
            Some(Path::new("/home/u")),
        )
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn config_parses_every_key() {
        let (config, diagnostics) = parse(
            "# theme\n\
             font = ~/fonts/Mono.ttf\n\
             font_size = 15.5\n\
             shell = /bin/bash\n\
             scrollback = 5000\n\
             foreground = #ABCDEF\n\
             background = #000000\n\
             cursor = #ff8800\n\
             color0 = #010203\n\
             color15=#0a0b0c\n",
        );
        assert_eq!(diagnostics, Vec::<String>::new());
        assert_eq!(config.font, Some(PathBuf::from("/home/u/fonts/Mono.ttf")));
        assert_eq!(config.font_size, Some(15.5));
        assert_eq!(config.shell, Some(OsString::from("/bin/bash")));
        assert_eq!(config.scrollback, 5000);
        assert_eq!(config.palette.foreground, [0xab, 0xcd, 0xef]);
        assert_eq!(config.palette.background, [0, 0, 0]);
        assert_eq!(config.palette.cursor, Some([0xff, 0x88, 0x00]));
        assert_eq!(config.palette.ansi[0], [1, 2, 3]);
        assert_eq!(config.palette.ansi[15], [10, 11, 12]);
        assert_eq!(config.palette.ansi[1], Palette::default().ansi[1]);
    }

    #[test]
    fn config_reports_bad_lines_and_keeps_defaults() {
        let (config, diagnostics) = parse(
            "font_size = 100\n\
             no equals here\n\
             colour1 = #ffffff\n\
             color16 = #ffffff\n\
             background = red\n\
             scrollback = 100001\n\
             shell =\n\
             foreground = #fff\n",
        );
        assert_eq!(
            diagnostics,
            vec![
                "nulix: config /cfg/nulix.conf:1: font_size: expected a number from 6 to 72, got \"100\"",
                "nulix: config /cfg/nulix.conf:2: expected KEY = VALUE",
                "nulix: config /cfg/nulix.conf:3: unknown key: colour1",
                "nulix: config /cfg/nulix.conf:4: unknown key: color16",
                "nulix: config /cfg/nulix.conf:5: background: expected #rrggbb, got \"red\"",
                "nulix: config /cfg/nulix.conf:6: scrollback: expected 0 to 100000 lines, got \"100001\"",
                "nulix: config /cfg/nulix.conf:7: shell: empty value",
                "nulix: config /cfg/nulix.conf:8: foreground: expected #rrggbb, got \"#fff\"",
            ]
        );
        assert_eq!(config, Config::default());
    }

    #[test]
    fn config_location_prefers_explicit_then_xdg_then_home() {
        assert_eq!(
            config_location(env(&[
                ("NULIX_CONFIG", "/x.conf"),
                ("XDG_CONFIG_HOME", "/xdg"),
                ("HOME", "/h")
            ])),
            Some(ConfigLocation {
                path: PathBuf::from("/x.conf"),
                explicit: true
            })
        );
        assert_eq!(
            config_location(env(&[
                ("NULIX_CONFIG", ""),
                ("XDG_CONFIG_HOME", "/xdg"),
                ("HOME", "/h")
            ])),
            Some(ConfigLocation {
                path: PathBuf::from("/xdg/nulix/nulix.conf"),
                explicit: false
            })
        );
        assert_eq!(
            config_location(env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/h")])),
            Some(ConfigLocation {
                path: PathBuf::from("/h/.config/nulix/nulix.conf"),
                explicit: false
            })
        );
        assert_eq!(config_location(env(&[])), None);
    }

    #[test]
    fn config_missing_default_file_is_silent_but_explicit_file_warns() {
        let dir = std::env::temp_dir().join(format!("nulix-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dir_text = dir.to_str().unwrap();
        assert_eq!(
            load_config(env(&[("XDG_CONFIG_HOME", dir_text)])),
            (Config::default(), Vec::new())
        );
        let missing = dir.join("missing.conf");
        let (config, diagnostics) =
            load_config(env(&[("NULIX_CONFIG", missing.to_str().unwrap())]));
        assert_eq!(config, Config::default());
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0].starts_with(&format!("nulix: config {}: ", missing.display())),
            "{diagnostics:?}"
        );

        std::fs::create_dir_all(dir.join("nulix")).unwrap();
        std::fs::write(dir.join("nulix/nulix.conf"), "scrollback = 42\n").unwrap();
        let (config, diagnostics) = load_config(env(&[("XDG_CONFIG_HOME", dir_text)]));
        assert_eq!(diagnostics, Vec::<String>::new());
        assert_eq!(config.scrollback, 42);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_example_file_parses_to_the_built_in_defaults() {
        let (config, diagnostics) = parse(include_str!("../../../docs/nulix.conf.example"));
        assert_eq!(diagnostics, Vec::<String>::new());
        assert_eq!(config, Config::default());
    }

    #[test]
    fn config_accepts_crlf_bom_equals_in_values_and_last_valid_duplicate() {
        let (config, diagnostics) = parse(
            "\u{feff}scrollback = 5\r\nforeground = #010203\r\nshell = /bin/a=b\r\nscrollback = x\r\n",
        );
        assert_eq!(
            diagnostics,
            vec![
                "nulix: config /cfg/nulix.conf:4: scrollback: expected 0 to 100000 lines, got \"x\""
            ]
        );
        assert_eq!(config.scrollback, 5);
        assert_eq!(config.palette.foreground, [1, 2, 3]);
        assert_eq!(config.shell, Some(OsString::from("/bin/a=b")));
    }

    #[test]
    fn config_value_limits_match_the_documented_ranges() {
        for (text, accepted) in [
            ("font_size = 6", true),
            ("font_size = 72", true),
            ("font_size = 1e1", true),
            ("font_size = 5.99", false),
            ("font_size = nan", false),
            ("font_size = inf", false),
            ("scrollback = 0", true),
            ("scrollback = 100000", true),
            ("scrollback = -1", false),
            ("color1 = #FF0000", true),
            ("color1 = #ff0000 # red", false),
            ("color01 = #ff0000", false),
            ("color+1 = #ff0000", false),
        ] {
            let (_, diagnostics) = parse(text);
            assert_eq!(diagnostics.is_empty(), accepted, "{text}: {diagnostics:?}");
        }
    }
}

//! Startup system banner and the `sysinfo` builtin (see odd/tasks/shell-prompt-banner.md, T2).
//!
//! Gathering is split from rendering: `Sources` holds the raw text of
//! `/etc/os-release`, `/proc/meminfo`, `/proc/cpuinfo` and `/proc/uptime`
//! plus `uname`, hostname and `statvfs("/")` results, so parsing is tested
//! with fixtures. `collect` reads the real sources and never fails: a field
//! that cannot be read is omitted from the banner.

use std::ffi::CStr;

use epsh::error::ExitStatus;
use epsh::eval::Shell;
use nu_ansi_term::{Color, Style};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// Spaces between the widest logo line and the info column.
const LOGO_GAP: usize = 5;
/// Width of the longest label (`Disk /:`) plus one space.
const LABEL_WIDTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Usage {
    pub used: u64,
    pub total: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct OsRelease {
    pub pretty_name: Option<String>,
    pub id: Option<String>,
    pub id_like: Vec<String>,
}

/// Raw, injectable inputs for `SystemInfo::from_sources`.
#[derive(Debug, Default, Clone)]
pub(crate) struct Sources {
    pub os_release: Option<String>,
    pub meminfo: Option<String>,
    pub cpuinfo: Option<String>,
    pub uptime: Option<String>,
    pub kernel: Option<String>,
    pub hostname: Option<String>,
    pub disk_root: Option<Usage>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct SystemInfo {
    pub user: Option<String>,
    pub host: Option<String>,
    pub os_name: Option<String>,
    pub os_id: Option<String>,
    pub os_id_like: Vec<String>,
    pub kernel: Option<String>,
    pub shell: String,
    pub terminal: Option<String>,
    pub cpu: Option<String>,
    pub ram: Option<Usage>,
    pub disk: Option<Usage>,
    pub uptime_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Logo {
    Debian,
    Ubuntu,
    Arch,
    Fedora,
    Generic,
}

const DEBIAN_LOGO: &[&str] = &[
    r"   _____",
    r"  /  __ \",
    r" |  /    |",
    r" |  \___-",
    r" -_",
    r"   --_",
];

const UBUNTU_LOGO: &[&str] = &[
    r"          _",
    r"      ---(_)",
    r"  _/  ---  \",
    r" (_) |   |",
    r"   \  --- _/",
    r"      ---(_)",
];

const ARCH_LOGO: &[&str] = &[
    r"      /\",
    r"     /  \",
    r"    /\   \",
    r"   /      \",
    r"  /   ,,   \",
    r" /   |  |  -\",
    r"/_-''    ''-_\",
];

const FEDORA_LOGO: &[&str] = &[
    r"      _____",
    r"     /   __)\",
    r"     |  /  \ \",
    r"  ___|  |__/ /",
    r" / (_    _)_/",
    r"/ /  |  |",
    r"\ \__/  |",
    r" \(_____/",
];

const GENERIC_LOGO: &[&str] = &[
    r"   /\",
    r"  /  \",
    r" /    \",
    r"/  /\  \",
    r"\  \/  /",
    r" \    /",
    r"  \  /",
    r"   \/",
];

impl Logo {
    /// Pick a logo from os-release `ID`, then each `ID_LIKE` entry in order.
    pub(crate) fn select(id: Option<&str>, id_like: &[String]) -> Logo {
        id.into_iter()
            .chain(id_like.iter().map(String::as_str))
            .find_map(|candidate| match candidate {
                "debian" => Some(Logo::Debian),
                "ubuntu" => Some(Logo::Ubuntu),
                "arch" => Some(Logo::Arch),
                "fedora" => Some(Logo::Fedora),
                _ => None,
            })
            .unwrap_or(Logo::Generic)
    }

    fn lines(self) -> &'static [&'static str] {
        match self {
            Logo::Debian => DEBIAN_LOGO,
            Logo::Ubuntu => UBUNTU_LOGO,
            Logo::Arch => ARCH_LOGO,
            Logo::Fedora => FEDORA_LOGO,
            Logo::Generic => GENERIC_LOGO,
        }
    }

    fn color(self) -> Color {
        match self {
            Logo::Debian => Color::Red,
            Logo::Ubuntu => Color::Yellow,
            Logo::Arch => Color::Cyan,
            Logo::Fedora => Color::Blue,
            Logo::Generic => Color::Green,
        }
    }
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

pub(crate) fn parse_os_release(text: &str) -> OsRelease {
    let mut release = OsRelease::default();
    let mut name = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unquote(value);
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "PRETTY_NAME" => release.pretty_name = Some(value.to_string()),
            "NAME" => name = Some(value.to_string()),
            "ID" => release.id = Some(value.to_ascii_lowercase()),
            "ID_LIKE" => {
                release.id_like = value
                    .split_whitespace()
                    .map(str::to_ascii_lowercase)
                    .collect();
            }
            _ => {}
        }
    }
    if release.pretty_name.is_none() {
        release.pretty_name = name;
    }
    release
}

/// RAM usage as `MemTotal - MemAvailable`; `None` unless both are present.
pub(crate) fn parse_meminfo(text: &str) -> Option<Usage> {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let (key, rest) = line.split_once(':')?;
            (key.trim() == name)
                .then(|| rest.split_whitespace().next()?.parse::<u64>().ok())
                .flatten()
        })
    };
    let total = field("MemTotal")?.checked_mul(1024)?;
    let available = field("MemAvailable")?.checked_mul(1024)?;
    Some(Usage {
        used: total.saturating_sub(available),
        total,
    })
}

/// CPU model with the logical processor count, for example `Model (12)`.
pub(crate) fn parse_cpuinfo(text: &str) -> Option<String> {
    let mut model = None;
    let mut count = 0usize;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "processor" => count += 1,
            "model name" | "Model" | "Hardware" | "cpu model" if model.is_none() => {
                let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
                if !value.is_empty() {
                    model = Some(value);
                }
            }
            _ => {}
        }
    }
    let model = model?;
    Some(if count > 0 {
        format!("{model} ({count})")
    } else {
        model
    })
}

/// Whole seconds from the first field of `/proc/uptime`.
pub(crate) fn parse_uptime(text: &str) -> Option<u64> {
    let seconds = text.split_whitespace().next()?.parse::<f64>().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then_some(seconds as u64)
}

pub(crate) fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// `used GiB / total GiB (percent%)` with one decimal; `None` for a zero total.
pub(crate) fn format_usage(usage: Usage) -> Option<String> {
    if usage.total == 0 {
        return None;
    }
    let total = u128::from(usage.total);
    let percent = (u128::from(usage.used) * 100 + total / 2) / total;
    Some(format!(
        "{:.1} GiB / {:.1} GiB ({percent}%)",
        usage.used as f64 / GIB,
        usage.total as f64 / GIB
    ))
}

impl SystemInfo {
    pub(crate) fn from_sources(sources: &Sources, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let os = sources
            .os_release
            .as_deref()
            .map(parse_os_release)
            .unwrap_or_default();
        let var = |name: &str| env(name).filter(|value| !value.is_empty());
        let terminal = if var("NULIX_SOCKET").is_some() {
            Some("nulix".to_string())
        } else {
            var("TERM_PROGRAM").or_else(|| var("TERM"))
        };
        SystemInfo {
            user: var("USER").or_else(|| var("LOGNAME")),
            host: sources.hostname.clone().filter(|host| !host.is_empty()),
            os_name: os.pretty_name,
            os_id: os.id,
            os_id_like: os.id_like,
            kernel: sources.kernel.clone().filter(|kernel| !kernel.is_empty()),
            shell: format!("shellux {}", env!("CARGO_PKG_VERSION")),
            terminal,
            cpu: sources.cpuinfo.as_deref().and_then(parse_cpuinfo),
            ram: sources.meminfo.as_deref().and_then(parse_meminfo),
            disk: sources.disk_root,
            uptime_seconds: sources.uptime.as_deref().and_then(parse_uptime),
        }
    }
}

/// Render the logo-left / info-right banner. Alignment is computed on the
/// plain text, so `color` only adds ANSI styling around the same characters.
pub(crate) fn render_banner(info: &SystemInfo, color: bool) -> String {
    let logo = Logo::select(info.os_id.as_deref(), &info.os_id_like);
    let accent = Style::new().fg(logo.color()).bold();
    let paint = |text: &str| {
        if color && !text.is_empty() {
            accent.paint(text).to_string()
        } else {
            text.to_string()
        }
    };

    let mut rows = Vec::new();
    let title = match (&info.user, &info.host) {
        (Some(user), Some(host)) => Some((
            format!("{user}@{host}"),
            format!("{}@{}", paint(user), paint(host)),
        )),
        (Some(name), None) | (None, Some(name)) => Some((name.clone(), paint(name))),
        (None, None) => None,
    };
    if let Some((plain, painted)) = title {
        rows.push(painted);
        rows.push("-".repeat(plain.chars().count()));
    }
    let fields = [
        ("OS:", info.os_name.clone()),
        ("Kernel:", info.kernel.clone()),
        ("Shell:", Some(info.shell.clone())),
        ("Term:", info.terminal.clone()),
        ("CPU:", info.cpu.clone()),
        ("RAM:", info.ram.and_then(format_usage)),
        ("Disk /:", info.disk.and_then(format_usage)),
        ("Uptime:", info.uptime_seconds.map(format_uptime)),
    ];
    for (label, value) in fields {
        if let Some(value) = value {
            let padding = " ".repeat(LABEL_WIDTH.saturating_sub(label.len()));
            rows.push(format!("{}{padding}{}", paint(label), value.trim()));
        }
    }

    let logo_lines = logo.lines();
    let width = logo_lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for index in 0..logo_lines.len().max(rows.len()) {
        let art = logo_lines.get(index).copied().unwrap_or("").trim_end();
        match rows.get(index) {
            Some(row) => {
                let padding = width - art.chars().count() + LOGO_GAP;
                out.push_str(&paint(art));
                out.push_str(&" ".repeat(padding));
                out.push_str(row);
            }
            None => out.push_str(&paint(art)),
        }
        out.push('\n');
    }
    out
}

/// The startup banner is shown unless `SHELLUX_BANNER` is exactly `0`.
pub(crate) fn banner_enabled(value: Option<&str>) -> bool {
    value != Some("0")
}

/// Color only when fd 1 is a terminal and `NO_COLOR` is unset or empty.
fn color_enabled(shell: &Shell) -> bool {
    shell.get_var("NO_COLOR").is_none_or(str::is_empty)
        // SAFETY: isatty only inspects the descriptor.
        && unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1
}

fn shell_banner(shell: &Shell) -> String {
    let info = collect(&|name| shell.get_var(name).map(str::to_owned));
    render_banner(&info, color_enabled(shell))
}

/// Register the `sysinfo` builtin, which prints the banner on demand.
pub fn register_sysinfo_builtin(shell: &mut Shell) -> Result<(), String> {
    shell.register_embedder_builtin("sysinfo", |shell, _argv| {
        let banner = shell_banner(shell);
        shell.write_stdout(banner.as_bytes());
        Ok(ExitStatus::SUCCESS)
    })
}

/// Print the startup banner unless the shell variable `SHELLUX_BANNER` is `0`.
/// Call after the rc file has been sourced so the rc can disable it.
pub fn show_startup_banner(shell: &Shell) {
    if banner_enabled(shell.get_var("SHELLUX_BANNER")) {
        shell.write_stdout(shell_banner(shell).as_bytes());
    }
}

/// Read the real system sources. Unreadable sources are left out.
pub(crate) fn collect(env: &dyn Fn(&str) -> Option<String>) -> SystemInfo {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let sources = Sources {
        os_release: read("/etc/os-release").or_else(|| read("/usr/lib/os-release")),
        meminfo: read("/proc/meminfo"),
        cpuinfo: read("/proc/cpuinfo"),
        uptime: read("/proc/uptime"),
        kernel: kernel_release(),
        hostname: hostname(),
        disk_root: root_disk_usage(),
    };
    #[allow(unused_mut)]
    let mut info = SystemInfo::from_sources(&sources, env);
    #[cfg(target_os = "macos")]
    macos::fill(&mut info);
    info
}

fn kernel_release() -> Option<String> {
    // SAFETY: utsname is plain data; uname fills NUL-terminated fields on success.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut name) } != 0 {
        return None;
    }
    let release = unsafe { CStr::from_ptr(name.release.as_ptr()) };
    release
        .to_str()
        .ok()
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
}

fn hostname() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is writable for its full length.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return None;
    }
    let end = buffer.iter().position(|&byte| byte == 0)?;
    String::from_utf8(buffer[..end].to_vec())
        .ok()
        .filter(|name| !name.is_empty())
}

// statvfs field widths differ by platform (u32 on macOS, u64 on Linux).
#[allow(clippy::useless_conversion)]
fn root_disk_usage() -> Option<Usage> {
    // SAFETY: statvfs is plain data and the path is a valid C string.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c"/".as_ptr(), &mut stats) } != 0 {
        return None;
    }
    let fragment = u64::from(stats.f_frsize);
    let total = u64::from(stats.f_blocks).checked_mul(fragment)?;
    let free = u64::from(stats.f_bfree).checked_mul(fragment)?;
    (total > 0).then(|| Usage {
        used: total.saturating_sub(free),
        total,
    })
}

/// Best-effort macOS fields through sysctl; RAM usage is omitted because it
/// needs Mach host statistics rather than a cheap sysctl.
#[cfg(target_os = "macos")]
mod macos {
    use super::SystemInfo;
    use std::ffi::CStr;

    fn sysctl_bytes(name: &CStr) -> Option<Vec<u8>> {
        let mut len: libc::size_t = 0;
        // SAFETY: a null buffer asks sysctl for the value length only.
        let probe = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if probe != 0 || len == 0 {
            return None;
        }
        let mut buffer = vec![0u8; len];
        // SAFETY: the buffer is writable for `len` bytes.
        let read = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if read != 0 {
            return None;
        }
        buffer.truncate(len);
        Some(buffer)
    }

    fn sysctl_string(name: &CStr) -> Option<String> {
        let bytes = sysctl_bytes(name)?;
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let value = String::from_utf8(bytes[..end].to_vec()).ok()?;
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    }

    fn sysctl_u64(name: &CStr) -> Option<u64> {
        let bytes = sysctl_bytes(name)?;
        match bytes.len() {
            8 => Some(u64::from_ne_bytes(bytes.try_into().ok()?)),
            4 => Some(u64::from(u32::from_ne_bytes(bytes.try_into().ok()?))),
            _ => None,
        }
    }

    pub(super) fn fill(info: &mut SystemInfo) {
        if info.os_name.is_none() {
            info.os_name = sysctl_string(c"kern.osproductversion").map(|v| format!("macOS {v}"));
        }
        if info.cpu.is_none() {
            info.cpu = sysctl_string(c"machdep.cpu.brand_string").map(|model| {
                match sysctl_u64(c"hw.logicalcpu") {
                    Some(count) => format!("{model} ({count})"),
                    None => model,
                }
            });
        }
        if info.uptime_seconds.is_none() {
            let boot = sysctl_bytes(c"kern.boottime")
                .and_then(|bytes| Some(i64::from_ne_bytes(bytes.get(..8)?.try_into().ok()?)));
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|elapsed| elapsed.as_secs());
            if let (Some(boot), Some(now)) = (boot, now) {
                info.uptime_seconds = u64::try_from(boot).ok().and_then(|b| now.checked_sub(b));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const DEBIAN_OS_RELEASE: &str = "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n\
        NAME=\"Debian GNU/Linux\"\nVERSION_ID=\"13\"\n# comment\nID=debian\n";
    const MINT_OS_RELEASE: &str = "NAME='Linux Mint'\nID=linuxmint\nID_LIKE=\"ubuntu debian\"\n";
    const MEMINFO: &str = "MemTotal:       16252928 kB\nMemFree:         1000000 kB\n\
        MemAvailable:   10800000 kB\nBuffers:          200000 kB\n";
    const CPUINFO: &str = "processor\t: 0\nmodel name\t: AMD Ryzen 5  5600 6-Core Processor\n\n\
        processor\t: 1\nmodel name\t: AMD Ryzen 5  5600 6-Core Processor\n\n\
        processor\t: 2\nmodel name\t: AMD Ryzen 5  5600 6-Core Processor\n";
    const GIB: u64 = 1024 * 1024 * 1024;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn fixture_sources() -> Sources {
        Sources {
            os_release: Some(DEBIAN_OS_RELEASE.to_string()),
            meminfo: Some(MEMINFO.to_string()),
            cpuinfo: Some(CPUINFO.to_string()),
            uptime: Some("11520.42 40000.00\n".to_string()),
            kernel: Some("6.12.113+deb13-amd64".to_string()),
            hostname: Some("localhost".to_string()),
            disk_root: Some(Usage {
                used: 120 * GIB,
                total: 457 * GIB,
            }),
        }
    }

    fn fixture_env(name: &str) -> Option<String> {
        match name {
            "USER" => Some("yimy".to_string()),
            "NULIX_SOCKET" => Some("/run/user/1000/nulix/nulix-1.sock".to_string()),
            "TERM" => Some("xterm-256color".to_string()),
            _ => None,
        }
    }

    #[test]
    fn sysinfo_parses_os_release_pretty_name_id_and_id_like() {
        let debian = parse_os_release(DEBIAN_OS_RELEASE);
        assert_eq!(
            debian.pretty_name.as_deref(),
            Some("Debian GNU/Linux 13 (trixie)")
        );
        assert_eq!(debian.id.as_deref(), Some("debian"));
        assert!(debian.id_like.is_empty());

        let mint = parse_os_release(MINT_OS_RELEASE);
        assert_eq!(mint.pretty_name.as_deref(), Some("Linux Mint"));
        assert_eq!(mint.id.as_deref(), Some("linuxmint"));
        assert_eq!(mint.id_like, vec!["ubuntu", "debian"]);
    }

    #[test]
    fn sysinfo_parses_meminfo_as_total_minus_available() {
        let usage = parse_meminfo(MEMINFO).unwrap();
        assert_eq!(usage.total, 16_252_928 * 1024);
        assert_eq!(usage.used, (16_252_928 - 10_800_000) * 1024);
        assert_eq!(
            format_usage(usage).as_deref(),
            Some("5.2 GiB / 15.5 GiB (34%)")
        );
        assert_eq!(parse_meminfo("MemTotal: 100 kB\n"), None);
        assert_eq!(parse_meminfo("garbage"), None);
    }

    #[test]
    fn sysinfo_formats_sizes_in_gib_with_one_decimal() {
        let disk = Usage {
            used: 120 * GIB,
            total: 457 * GIB,
        };
        assert_eq!(
            format_usage(disk).as_deref(),
            Some("120.0 GiB / 457.0 GiB (26%)")
        );
        assert_eq!(format_usage(Usage { used: 0, total: 0 }), None);
    }

    #[test]
    fn sysinfo_parses_cpu_model_and_logical_count() {
        assert_eq!(
            parse_cpuinfo(CPUINFO).as_deref(),
            Some("AMD Ryzen 5 5600 6-Core Processor (3)")
        );
        let arm = "processor\t: 0\nprocessor\t: 1\nModel\t\t: Raspberry Pi 4 Model B\n";
        assert_eq!(
            parse_cpuinfo(arm).as_deref(),
            Some("Raspberry Pi 4 Model B (2)")
        );
        assert_eq!(parse_cpuinfo("processor\t: 0\n"), None);
    }

    #[test]
    fn sysinfo_parses_and_formats_uptime() {
        assert_eq!(parse_uptime("11520.42 40000.00\n"), Some(11520));
        assert_eq!(parse_uptime("nonsense"), None);
        assert_eq!(format_uptime(3 * 3600 + 12 * 60 + 5), "3h 12m");
        assert_eq!(format_uptime(2 * 86400 + 4 * 3600 + 59), "2d 4h");
        assert_eq!(format_uptime(5 * 60 + 30), "5m");
        assert_eq!(format_uptime(10), "0m");
    }

    #[test]
    fn sysinfo_selects_logo_from_id_then_id_like() {
        let like = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        assert_eq!(Logo::select(Some("debian"), &[]), Logo::Debian);
        assert_eq!(
            Logo::select(Some("ubuntu"), &like(&["debian"])),
            Logo::Ubuntu
        );
        assert_eq!(Logo::select(Some("arch"), &[]), Logo::Arch);
        assert_eq!(Logo::select(Some("fedora"), &[]), Logo::Fedora);
        assert_eq!(
            Logo::select(Some("linuxmint"), &like(&["ubuntu", "debian"])),
            Logo::Ubuntu
        );
        assert_eq!(Logo::select(Some("manjaro"), &like(&["arch"])), Logo::Arch);
        assert_eq!(Logo::select(Some("macos"), &[]), Logo::Generic);
        assert_eq!(Logo::select(None, &[]), Logo::Generic);
    }

    #[test]
    fn sysinfo_builds_info_from_sources_and_env() {
        let info = SystemInfo::from_sources(&fixture_sources(), &fixture_env);
        assert_eq!(info.user.as_deref(), Some("yimy"));
        assert_eq!(info.host.as_deref(), Some("localhost"));
        assert_eq!(
            info.os_name.as_deref(),
            Some("Debian GNU/Linux 13 (trixie)")
        );
        assert_eq!(info.os_id.as_deref(), Some("debian"));
        assert_eq!(info.kernel.as_deref(), Some("6.12.113+deb13-amd64"));
        assert_eq!(info.shell, format!("shellux {}", env!("CARGO_PKG_VERSION")));
        assert_eq!(info.terminal.as_deref(), Some("nulix"));
        assert_eq!(info.uptime_seconds, Some(11520));
        assert!(info.cpu.is_some() && info.ram.is_some() && info.disk.is_some());

        let term_program = |name: &str| match name {
            "TERM_PROGRAM" => Some("WezTerm".to_string()),
            "TERM" => Some("xterm".to_string()),
            "NULIX_SOCKET" => Some(String::new()),
            _ => None,
        };
        let info = SystemInfo::from_sources(&Sources::default(), &term_program);
        assert_eq!(info.terminal.as_deref(), Some("WezTerm"));
        let term_only = |name: &str| (name == "TERM").then(|| "xterm".to_string());
        let info = SystemInfo::from_sources(&Sources::default(), &term_only);
        assert_eq!(info.terminal.as_deref(), Some("xterm"));
    }

    #[test]
    fn sysinfo_renders_logo_left_and_aligned_info_right() {
        let info = SystemInfo::from_sources(&fixture_sources(), &fixture_env);
        let banner = render_banner(&info, false);
        let lines = banner.lines().collect::<Vec<_>>();
        let column = lines[0].find("yimy@localhost").expect("title line");
        assert!(column > 0);
        let expected = [
            "yimy@localhost",
            "--------------",
            "OS:     Debian GNU/Linux 13 (trixie)",
            "Kernel: 6.12.113+deb13-amd64",
            &format!("Shell:  shellux {}", env!("CARGO_PKG_VERSION")),
            "Term:   nulix",
            "CPU:    AMD Ryzen 5 5600 6-Core Processor (3)",
            "RAM:    5.2 GiB / 15.5 GiB (34%)",
            "Disk /: 120.0 GiB / 457.0 GiB (26%)",
            "Uptime: 3h 12m",
        ];
        assert!(lines.len() >= expected.len());
        for (line, info) in lines.iter().zip(expected) {
            assert_eq!(&line[column..], info, "line {line:?}");
        }
        // The logo occupies the left column of the first line.
        assert!(!lines[0][..column].trim().is_empty());
        assert!(banner.ends_with('\n'));
        assert!(!banner.contains('\u{1b}'));
        assert!(banner.lines().all(|line| line == line.trim_end()));
    }

    #[test]
    fn sysinfo_omits_unreadable_fields() {
        let info = SystemInfo::from_sources(&Sources::default(), &no_env);
        let banner = render_banner(&info, false);
        assert!(banner.contains("Shell:  shellux"));
        for label in [
            "OS:", "Kernel:", "Term:", "CPU:", "RAM:", "Disk /:", "Uptime:", "---",
        ] {
            assert!(!banner.contains(label), "{label} in {banner}");
        }
    }

    #[test]
    fn sysinfo_colors_only_when_requested() {
        let info = SystemInfo::from_sources(&fixture_sources(), &fixture_env);
        let colored = render_banner(&info, true);
        assert!(colored.contains('\u{1b}'));
        let stripped = String::from_utf8(strip_ansi_escapes::strip(&colored)).unwrap();
        assert_eq!(stripped, render_banner(&info, false));
    }

    #[test]
    fn sysinfo_banner_is_disabled_only_by_zero() {
        assert!(banner_enabled(None));
        assert!(banner_enabled(Some("")));
        assert!(banner_enabled(Some("1")));
        assert!(!banner_enabled(Some("0")));
    }

    #[test]
    fn sysinfo_startup_banner_respects_shellux_banner_variable() {
        let sink = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .env_clear()
            .stdout_sink(sink.clone())
            .build();
        shell.set_var("SHELLUX_BANNER", "0").unwrap();
        show_startup_banner(&shell);
        assert!(sink.lock().unwrap().is_empty());

        shell.set_var("SHELLUX_BANNER", "1").unwrap();
        shell.set_var("NO_COLOR", "1").unwrap();
        show_startup_banner(&shell);
        let output = String::from_utf8(sink.lock().unwrap().clone()).unwrap();
        assert!(output.contains("Shell:  shellux"), "{output}");
        assert!(!output.contains('\u{1b}'));
    }

    #[test]
    fn sysinfo_builtin_writes_banner_to_stdout() {
        let sink = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .env_clear()
            .stdout_sink(sink.clone())
            .build();
        register_sysinfo_builtin(&mut shell).unwrap();
        shell.set_var("NO_COLOR", "1").unwrap();
        assert_eq!(shell.run_script("sysinfo"), ExitStatus::SUCCESS.code());
        let output = String::from_utf8(sink.lock().unwrap().clone()).unwrap();
        assert!(output.contains("Shell:  shellux"), "{output}");
        assert!(!output.contains('\u{1b}'));
        assert_eq!(shell.run_script("command -v sysinfo"), 0);
    }

    #[test]
    #[ignore = "prints the banner from the real collector; run with --ignored --nocapture"]
    fn sysinfo_real_collector_sample() {
        let info = collect(&|name| std::env::var(name).ok());
        print!("{}", render_banner(&info, false));
    }
}

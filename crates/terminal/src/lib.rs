pub mod block_navigation;
pub mod blocks;
pub mod model;
pub mod osc;
pub mod pty;
pub mod renderer;
pub mod selection;

/// Default interactive-shell command candidates, in preference order.
pub fn default_shell_candidates() -> &'static [&'static str] {
    #[cfg(unix)]
    {
        &["so1sh"]
    }
    #[cfg(windows)]
    {
        &["pwsh", "cmd"]
    }
    #[cfg(not(any(unix, windows)))]
    {
        &[]
    }
}

pub fn default_shell_candidates_for(windows: bool) -> &'static [&'static str] {
    if windows {
        &["pwsh", "cmd"]
    } else {
        &["so1sh"]
    }
}

/// Pure decision logic; production supplies the environment, sibling existence, and PATH probes.
pub fn resolve_shell_choice<'a>(
    override_path: Option<&'a str>,
    sibling_exists: bool,
    candidates: &'a [&'a str],
) -> &'a str {
    if let Some(path) = override_path {
        return path;
    }
    if sibling_exists {
        return "sibling-so1sh";
    }
    candidates.first().copied().unwrap_or("so1sh")
}

/// Surface dimensions must be non-zero even when a window is minimized.
pub fn clamp_surface_size(width: u32, height: u32) -> (u32, u32) {
    (width.max(1), height.max(1))
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_surface_size, default_shell_candidates, default_shell_candidates_for,
        resolve_shell_choice,
    };

    #[test]
    fn default_shell_candidates_are_platform_ordered() {
        #[cfg(unix)]
        assert_eq!(default_shell_candidates(), &["so1sh"]);
        #[cfg(windows)]
        assert_eq!(default_shell_candidates(), &["pwsh", "cmd"]);
    }

    #[test]
    fn surface_size_never_contains_zero_dimensions() {
        assert_eq!(clamp_surface_size(0, 0), (1, 1));
        assert_eq!(clamp_surface_size(1280, 0), (1280, 1));
        assert_eq!(clamp_surface_size(0, 720), (1, 720));
        assert_eq!(clamp_surface_size(1280, 720), (1280, 720));
    }

    #[test]
    fn shell_selection_prefers_override_then_sibling_then_path_candidates() {
        assert_eq!(
            resolve_shell_choice(Some("custom"), true, &["so1sh"]),
            "custom"
        );
        assert_eq!(
            resolve_shell_choice(None, true, &["so1sh"]),
            "sibling-so1sh"
        );
        assert_eq!(resolve_shell_choice(None, false, &["so1sh"]), "so1sh");
    }

    #[test]
    fn windows_shell_candidate_order_is_testable_on_any_host() {
        assert_eq!(resolve_shell_choice(None, false, &["pwsh", "cmd"]), "pwsh");
        assert_eq!(default_shell_candidates_for(true), &["pwsh", "cmd"]);
    }
}
pub mod keyboard;

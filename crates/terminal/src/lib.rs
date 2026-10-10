pub mod osc;

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

/// Surface dimensions must be non-zero even when a window is minimized.
pub fn clamp_surface_size(width: u32, height: u32) -> (u32, u32) {
    (width.max(1), height.max(1))
}

#[cfg(test)]
mod tests {
    use super::{clamp_surface_size, default_shell_candidates};

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
}

//! Shared display formatting utilities for truncating strings and paths.

/// Truncate a string to a maximum number of characters (not bytes).
pub(crate) fn truncate_string(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().take(max_chars + 1).collect();
    if chars.len() > max_chars {
        format!("{}...", chars[..max_chars].iter().collect::<String>())
    } else {
        s.to_string()
    }
}

/// Shorten a file path for display, showing the last `tail_components` components.
///
/// For paths longer than `tail_components`, prefixes with `...`.
pub(crate) fn shorten_path(path: &str) -> String {
    shorten_path_tail(path, 3)
}

/// Shorten a file path for display, showing the last N components.
///
/// # Panics
/// Debug-asserts that `tail_components > 0`.  In release builds a zero value
/// returns the full path unchanged (no truncation).
pub(crate) fn shorten_path_tail(path: &str, tail_components: usize) -> String {
    debug_assert!(tail_components > 0, "tail_components must be > 0");
    let path_obj = std::path::Path::new(path);
    let components: Vec<_> = path_obj.components().collect();

    if tail_components == 0 || components.len() <= tail_components {
        path.to_string()
    } else {
        let last_parts: Vec<_> = components
            .iter()
            .rev()
            .take(tail_components)
            .rev()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect();
        format!(".../{}", last_parts.join("/"))
    }
}

/// Shorten a path by replacing the home directory prefix with `~`.
///
/// Falls back to the full path display if the home directory cannot be determined
/// or the path is not under it.
pub(crate) fn shorten_path_home(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(suffix) = path.strip_prefix(&home) {
            return format!("~/{}", suffix.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_string_short() {
        assert_eq!(truncate_string("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_string_exact() {
        assert_eq!(truncate_string("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_string_long() {
        assert_eq!(truncate_string("hello world", 5), "hello...");
    }

    #[test]
    fn test_shorten_path_short() {
        assert_eq!(shorten_path("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_shorten_path_long() {
        assert_eq!(
            shorten_path("/Users/test/projects/gru/src/commands/fix.rs"),
            ".../src/commands/fix.rs"
        );
    }

    #[test]
    fn test_shorten_path_tail_custom() {
        assert_eq!(shorten_path_tail("/a/b/c/d/e", 2), ".../d/e");
    }

    #[test]
    fn test_shorten_path_home_with_home() {
        if let Some(home) = dirs::home_dir() {
            let full = home.join("some/path");
            assert_eq!(shorten_path_home(&full), "~/some/path");
        }
    }

    #[test]
    fn test_shorten_path_home_no_home_prefix() {
        let path = std::path::Path::new("/tmp/some/path");
        assert_eq!(shorten_path_home(path), "/tmp/some/path");
    }
}

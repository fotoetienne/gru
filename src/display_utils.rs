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

/// Shorten a file path for display, showing the last 3 components.
///
/// Note: This differs from `commands::clean::shorten_path` which replaces
/// the home directory prefix with `~`.
pub(crate) fn shorten_path(path: &str) -> String {
    let path_obj = std::path::Path::new(path);
    let components: Vec<_> = path_obj.components().collect();

    if components.len() <= 3 {
        path.to_string()
    } else {
        let last_parts: Vec<_> = components
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect();
        format!(".../{}", last_parts.join("/"))
    }
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
}

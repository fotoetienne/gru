/// Returns `true` if the input is an affirmative answer (empty, "y", or "yes").
///
/// Used by interactive confirmation prompts throughout the CLI.
pub fn is_affirmative(input: &str) -> bool {
    let answer = input.trim().to_lowercase();
    answer.is_empty() || answer == "y" || answer == "yes"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_affirmative_empty_input() {
        // Enter key (empty input) defaults to yes
        assert!(is_affirmative(""));
        assert!(is_affirmative("\n"));
        assert!(is_affirmative("  \n"));
    }

    #[test]
    fn test_is_affirmative_yes_variants() {
        assert!(is_affirmative("y\n"));
        assert!(is_affirmative("Y\n"));
        assert!(is_affirmative("yes\n"));
        assert!(is_affirmative("YES\n"));
        assert!(is_affirmative("Yes\n"));
        assert!(is_affirmative("  y  \n"));
    }

    #[test]
    fn test_is_affirmative_no_variants() {
        assert!(!is_affirmative("n\n"));
        assert!(!is_affirmative("N\n"));
        assert!(!is_affirmative("no\n"));
        assert!(!is_affirmative("NO\n"));
        assert!(!is_affirmative("nope\n"));
        assert!(!is_affirmative("anything else\n"));
    }
}

/// Reserved system commands that cannot be used as custom prompt names.
///
/// These commands are hardcoded in the CLI and must always resolve to their
/// system behavior first. Custom prompts cannot override these names.
///
/// Part of Phase 1: Custom Prompts (see plans/CUSTOM_PROMPTS_PRD.md)
///
/// **Status**: This module defines the reserved commands list and validation functions
/// ready for Phase 2 (Prompt Files) implementation. The actual prompt file loading
/// and validation will be added in a future phase.
use anyhow::{bail, Result};

/// List of reserved command names that cannot be used for custom prompts
///
/// From issue #79 and CUSTOM_PROMPTS_PRD.md resolution order:
/// 1. Reserved system commands (this list) - CANNOT be overridden
/// 2. Quoted strings → ad-hoc prompts
/// 3. Unquoted strings → prompt file lookup (can override built-ins)
///
/// **Important**: Commands like `fix`, `review`, `init`, etc. are NOT in this list
/// because they are built-in prompts (Phase 4) that teams can override with
/// custom versions in `.gru/prompts/`. Reserved commands are truly protected
/// system commands that must always resolve to their hardcoded CLI behavior.
///
/// This will be used by the prompt file loader (Phase 2) to validate that
/// `.gru/prompts/*.md` files don't use these reserved names.
#[cfg_attr(not(test), allow(dead_code))]
pub const RESERVED_COMMANDS: &[&str] = &[
    "status", "attach", "stop", "lab", "tower", "up", "prompts", "help", "version",
];

/// Validates that a given name is not a reserved command
///
/// This will be called by the prompt file loader (Phase 2) when loading
/// custom prompts from `.gru/prompts/*.md` to ensure they don't shadow
/// reserved system commands.
///
/// # Arguments
/// * `name` - The name to validate (e.g., a custom prompt name)
///
/// # Returns
/// * `Ok(())` if the name is not reserved
/// * `Err` with a helpful error message listing all reserved commands
#[cfg_attr(not(test), allow(dead_code))]
pub fn validate_not_reserved(name: &str) -> Result<()> {
    let lowercase_name = name.to_lowercase();

    if RESERVED_COMMANDS
        .iter()
        .any(|reserved| *reserved == lowercase_name)
    {
        bail!(
            "Error: '{}' is a reserved command and cannot be used as a prompt name.\n\n\
             Reserved commands: {}",
            name,
            RESERVED_COMMANDS.join(", ")
        );
    }

    Ok(())
}

/// Checks if a name is reserved (returns bool instead of Result)
///
/// Useful for conditional logic without error handling overhead.
///
/// This will be used by the prompt file loader (Phase 2) and potentially
/// other components that need to check if a name conflicts with reserved commands.
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_reserved(name: &str) -> bool {
    let lowercase_name = name.to_lowercase();
    RESERVED_COMMANDS
        .iter()
        .any(|reserved| *reserved == lowercase_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_not_reserved_accepts_custom_names() {
        // These should all pass
        assert!(validate_not_reserved("my-custom-prompt").is_ok());
        assert!(validate_not_reserved("fix").is_ok());
        assert!(validate_not_reserved("review").is_ok());
        assert!(validate_not_reserved("analyze").is_ok());
        assert!(validate_not_reserved("refactor").is_ok());
    }

    #[test]
    fn test_validate_not_reserved_rejects_reserved_names() {
        // Test each reserved command
        for reserved in RESERVED_COMMANDS {
            let result = validate_not_reserved(reserved);
            assert!(
                result.is_err(),
                "Expected '{}' to be rejected as reserved",
                reserved
            );

            let error_msg = result.unwrap_err().to_string();
            assert!(
                error_msg.contains(reserved),
                "Error message should mention the reserved name '{}'",
                reserved
            );
            assert!(
                error_msg.contains("Reserved commands:"),
                "Error message should list all reserved commands"
            );
        }
    }

    #[test]
    fn test_validate_not_reserved_case_insensitive() {
        // Reserved commands should be rejected regardless of case
        assert!(validate_not_reserved("STATUS").is_err());
        assert!(validate_not_reserved("Status").is_err());
        assert!(validate_not_reserved("StAtUs").is_err());
        assert!(validate_not_reserved("ATTACH").is_err());
        assert!(validate_not_reserved("Attach").is_err());
    }

    #[test]
    fn test_is_reserved() {
        // Test reserved names
        assert!(is_reserved("status"));
        assert!(is_reserved("attach"));
        assert!(is_reserved("lab"));
        assert!(is_reserved("tower"));

        // Test case insensitivity
        assert!(is_reserved("STATUS"));
        assert!(is_reserved("Status"));
        assert!(is_reserved("ATTACH"));

        // Test non-reserved names
        assert!(!is_reserved("fix"));
        assert!(!is_reserved("review"));
        assert!(!is_reserved("custom"));
        assert!(!is_reserved("my-prompt"));
    }

    #[test]
    fn test_error_message_format() {
        let result = validate_not_reserved("status");
        assert!(result.is_err());

        let error_msg = result.unwrap_err().to_string();

        // Check that error message follows the expected format from issue #79
        assert!(error_msg.contains("Error:"));
        assert!(error_msg.contains("'status'"));
        assert!(error_msg.contains("reserved command"));
        assert!(error_msg.contains("cannot be used as a prompt name"));
        assert!(error_msg.contains("Reserved commands:"));

        // Check that all reserved commands are listed
        for reserved in RESERVED_COMMANDS {
            assert!(
                error_msg.contains(reserved),
                "Error message should list '{}'",
                reserved
            );
        }
    }

    #[test]
    fn test_reserved_list_matches_issue_spec() {
        // Verify the list matches issue #79 acceptance criteria
        let expected = vec![
            "status", "attach", "stop", "lab", "tower", "up", "prompts", "help", "version",
        ];

        assert_eq!(
            RESERVED_COMMANDS.len(),
            expected.len(),
            "Reserved commands list length should match specification"
        );

        for cmd in expected {
            assert!(
                RESERVED_COMMANDS.contains(&cmd),
                "Expected '{}' to be in reserved commands list",
                cmd
            );
        }
    }
}

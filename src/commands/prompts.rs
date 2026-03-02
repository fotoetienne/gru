use anyhow::Result;

use crate::git;
use crate::prompt_loader::{self, BUILT_IN_PROMPTS};

/// Handles the `gru prompts` command by listing all available prompts
/// grouped by source (built-in, repo, global).
pub async fn handle_prompts() -> Result<i32> {
    let repo_root = git::detect_git_repo().await.ok();

    let prompts_by_source = prompt_loader::list_prompts_by_source(repo_root.as_deref())?;

    // Collect names of built-in prompts for override detection
    let built_in_names: Vec<&str> = BUILT_IN_PROMPTS.iter().map(|b| b.name).collect();

    // Only repo prompts can override built-ins (resolution order: repo > built-in > global).
    // Global prompts have lower priority than built-ins, so they don't override them.
    let repo_override_names: Vec<&str> = prompts_by_source
        .repo
        .iter()
        .filter(|p| built_in_names.contains(&p.name.as_str()))
        .map(|p| p.name.as_str())
        .collect();

    // Built-in prompts (always present)
    println!("BUILT-IN PROMPTS:");
    for (name, description) in &prompts_by_source.built_in {
        if repo_override_names.contains(&name.as_str()) {
            println!("  {:<16} {} [OVERRIDDEN]", name, description);
        } else {
            println!("  {:<16} {}", name, description);
        }
    }

    // Repo prompts (higher priority than built-ins)
    if !prompts_by_source.repo.is_empty() {
        println!();
        println!("CUSTOM PROMPTS (.gru/prompts/):");
        for prompt in &prompts_by_source.repo {
            let description = prompt
                .metadata
                .description
                .as_deref()
                .unwrap_or("(no description)");

            if built_in_names.contains(&prompt.name.as_str()) {
                println!("  {:<16} [OVERRIDES BUILT-IN] {}", prompt.name, description);
            } else {
                println!("  {:<16} {}", prompt.name, description);
            }
        }
    }

    // Global prompts (lower priority than built-ins)
    if !prompts_by_source.global.is_empty() {
        println!();
        println!("GLOBAL PROMPTS (~/.gru/prompts/):");
        for prompt in &prompts_by_source.global {
            let description = prompt
                .metadata
                .description
                .as_deref()
                .unwrap_or("(no description)");

            if built_in_names.contains(&prompt.name.as_str()) {
                println!(
                    "  {:<16} [SHADOWED BY BUILT-IN] {}",
                    prompt.name, description
                );
            } else {
                println!("  {:<16} {}", prompt.name, description);
            }
        }
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_built_in_prompts_defined() {
        // Verify built-in prompts have non-empty names and descriptions
        assert!(BUILT_IN_PROMPTS.len() >= 2);
        for builtin in BUILT_IN_PROMPTS {
            assert!(!builtin.name.is_empty());
            assert!(!builtin.description.is_empty());
        }
    }

    #[test]
    fn test_list_prompts_by_source_empty_dirs() {
        let temp_dir = TempDir::new().unwrap();
        // Use _internal to avoid reading real ~/.gru/prompts/
        let result = prompt_loader::list_prompts_by_source_internal(
            Some(temp_dir.path()),
            Some(temp_dir.path()),
        );
        assert!(result.is_ok());

        let prompts = result.unwrap();
        assert!(!prompts.built_in.is_empty()); // Built-ins always present
        assert!(prompts.repo.is_empty());
        assert!(prompts.global.is_empty());
    }

    #[test]
    fn test_list_prompts_by_source_with_repo_prompts() {
        let temp_dir = TempDir::new().unwrap();
        let prompts_dir = temp_dir.path().join(".gru").join("prompts");
        fs::create_dir_all(&prompts_dir).unwrap();

        fs::write(
            prompts_dir.join("analyze-deps.md"),
            "---\ndescription: Analyze dependency graph\n---\nAnalyze deps",
        )
        .unwrap();

        let result = prompt_loader::list_prompts_by_source_internal(
            Some(temp_dir.path()),
            Some(temp_dir.path()),
        );
        assert!(result.is_ok());

        let prompts = result.unwrap();
        assert_eq!(prompts.repo.len(), 1);
        assert_eq!(prompts.repo[0].name, "analyze-deps");
        assert_eq!(
            prompts.repo[0].metadata.description,
            Some("Analyze dependency graph".to_string())
        );
    }

    #[test]
    fn test_repo_prompt_overrides_built_in_detected() {
        let temp_dir = TempDir::new().unwrap();
        let prompts_dir = temp_dir.path().join(".gru").join("prompts");
        fs::create_dir_all(&prompts_dir).unwrap();

        // Create a prompt named "fix" which matches a built-in
        fs::write(
            prompts_dir.join("fix.md"),
            "---\ndescription: Team fix workflow\n---\nCustom fix",
        )
        .unwrap();

        let result = prompt_loader::list_prompts_by_source_internal(
            Some(temp_dir.path()),
            Some(temp_dir.path()),
        );
        assert!(result.is_ok());

        let prompts = result.unwrap();

        // The repo prompt named "fix" should be present
        assert_eq!(prompts.repo.len(), 1);
        assert_eq!(prompts.repo[0].name, "fix");

        // Built-in "fix" should still be listed
        let built_in_names: Vec<&str> = prompts.built_in.iter().map(|(n, _)| n.as_str()).collect();
        assert!(built_in_names.contains(&"fix"));
    }

    #[test]
    fn test_global_prompt_matching_builtin_is_not_override() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let global_dir = temp_dir.path().join("global");
        fs::create_dir_all(repo_dir.join(".gru").join("prompts")).unwrap();

        // Create a global prompt named "fix" which matches a built-in
        let global_prompts = global_dir.join(".gru").join("prompts");
        fs::create_dir_all(&global_prompts).unwrap();
        fs::write(
            global_prompts.join("fix.md"),
            "---\ndescription: Global fix override attempt\n---\nGlobal fix",
        )
        .unwrap();

        let result = prompt_loader::list_prompts_by_source_internal(
            Some(repo_dir.as_path()),
            Some(global_dir.as_path()),
        );
        assert!(result.is_ok());

        let prompts = result.unwrap();

        // Global prompt named "fix" should appear in the global collection
        assert_eq!(prompts.global.len(), 1);
        assert_eq!(prompts.global[0].name, "fix");

        // No repo prompts exist, so no built-in can be marked [OVERRIDDEN].
        // Only repo prompts override built-ins; global prompts are shadowed BY built-ins.
        assert!(prompts.repo.is_empty());
    }

    #[test]
    fn test_reserved_commands_excluded() {
        let temp_dir = TempDir::new().unwrap();
        let prompts_dir = temp_dir.path().join(".gru").join("prompts");
        fs::create_dir_all(&prompts_dir).unwrap();

        // Create a prompt with a reserved name
        fs::write(
            prompts_dir.join("status.md"),
            "---\ndescription: Should be filtered\n---\nContent",
        )
        .unwrap();

        // And a valid prompt
        fs::write(
            prompts_dir.join("valid.md"),
            "---\ndescription: Valid prompt\n---\nContent",
        )
        .unwrap();

        let result = prompt_loader::list_prompts_by_source_internal(
            Some(temp_dir.path()),
            Some(temp_dir.path()),
        );
        assert!(result.is_ok());

        let prompts = result.unwrap();
        // "status" should be filtered out, only "valid" should be present
        assert_eq!(prompts.repo.len(), 1);
        assert_eq!(prompts.repo[0].name, "valid");
    }
}

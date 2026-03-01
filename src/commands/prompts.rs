use anyhow::Result;

use crate::prompt_loader::{self, BUILT_IN_PROMPTS};

/// Handles the `gru prompts` command by listing all available prompts
/// grouped by source (built-in, repo, global).
pub async fn handle_prompts() -> Result<i32> {
    // Detect repo root from current working directory
    let repo_root = detect_repo_root();

    let prompts_by_source = prompt_loader::list_prompts_by_source(repo_root.as_deref())?;

    // Collect names of repo prompts that override built-in ones
    let built_in_names: Vec<&str> = BUILT_IN_PROMPTS.iter().map(|(name, _)| *name).collect();
    let repo_override_names: Vec<&str> = prompts_by_source
        .repo
        .iter()
        .filter(|p| built_in_names.contains(&p.name.as_str()))
        .map(|p| p.name.as_str())
        .collect();

    let mut any_output = false;

    // Built-in prompts
    if !prompts_by_source.built_in.is_empty() {
        println!("BUILT-IN PROMPTS:");
        for (name, description) in &prompts_by_source.built_in {
            if repo_override_names.contains(&name.as_str()) {
                println!("  {:<16} {} [OVERRIDDEN]", name, description);
            } else {
                println!("  {:<16} {}", name, description);
            }
        }
        any_output = true;
    }

    // Repo prompts
    if !prompts_by_source.repo.is_empty() {
        if any_output {
            println!();
        }
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
        any_output = true;
    }

    // Global prompts
    if !prompts_by_source.global.is_empty() {
        if any_output {
            println!();
        }
        println!("GLOBAL PROMPTS (~/.gru/prompts/):");
        for prompt in &prompts_by_source.global {
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
        any_output = true;
    }

    if !any_output {
        println!("No prompts found.");
        println!();
        println!("To create custom prompts, add .md files to:");
        println!("  .gru/prompts/     (repo-specific)");
        println!("  ~/.gru/prompts/   (global)");
    }

    Ok(0)
}

/// Detects the git repository root from the current working directory.
fn detect_repo_root() -> Option<std::path::PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Some(std::path::PathBuf::from(path))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_detect_repo_root_returns_some_in_git_repo() {
        // This test runs inside a git repo (the gru project itself)
        let root = detect_repo_root();
        assert!(root.is_some());
    }

    #[test]
    fn test_built_in_prompts_defined() {
        // Verify built-in prompts have non-empty names and descriptions
        assert!(BUILT_IN_PROMPTS.len() >= 2);
        for (name, desc) in BUILT_IN_PROMPTS {
            assert!(!name.is_empty());
            assert!(!desc.is_empty());
        }
    }

    #[test]
    fn test_list_prompts_by_source_empty_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let result = prompt_loader::list_prompts_by_source(Some(temp_dir.path()));
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

        let result = prompt_loader::list_prompts_by_source(Some(temp_dir.path()));
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

        let result = prompt_loader::list_prompts_by_source(Some(temp_dir.path()));
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

        let result = prompt_loader::list_prompts_by_source(Some(temp_dir.path()));
        assert!(result.is_ok());

        let prompts = result.unwrap();
        // "status" should be filtered out, only "valid" should be present
        assert_eq!(prompts.repo.len(), 1);
        assert_eq!(prompts.repo[0].name, "valid");
    }
}

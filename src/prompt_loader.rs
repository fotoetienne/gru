/// Module for loading custom prompt files from .gru/prompts/*.md
///
/// Implements Phase 2 of Custom Prompts feature (see plans/CUSTOM_PROMPTS_PRD.md)
///
/// Responsibilities:
/// - Load prompts from `.gru/prompts/*.md` (repo-specific)
/// - Load prompts from `~/.gru/prompts/*.md` (global)
/// - Parse YAML frontmatter for metadata
/// - Support `description`, `requires`, and `params` fields
/// - Resolution order: repo → built-in → global
/// - Validate prompt syntax on load
/// - List prompts grouped by source for `gru prompts` command
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::reserved_commands;

/// Built-in prompt definitions (commands that will become overridable templates in Phase 4)
pub const BUILT_IN_PROMPTS: &[(&str, &str)] = &[
    ("fix", "Fix a GitHub issue with tests and PR"),
    ("review", "Review and respond to PR comments"),
];

/// Prompts grouped by their source, for display in `gru prompts`
pub struct PromptsBySource {
    pub built_in: Vec<(String, String)>,
    pub repo: Vec<Prompt>,
    pub global: Vec<Prompt>,
}

/// Metadata for a prompt file, parsed from YAML frontmatter
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptMetadata {
    /// Short description of what the prompt does
    pub description: Option<String>,

    /// Context requirements (e.g., "issue", "pr")
    #[serde(default)]
    pub requires: Vec<String>,

    /// Parameter definitions for the prompt
    #[serde(default)]
    pub params: Vec<PromptParam>,
}

/// Definition of a prompt parameter
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptParam {
    /// Parameter name
    pub name: String,

    /// Description of what the parameter does
    pub description: Option<String>,

    /// Whether the parameter is required
    #[serde(default)]
    pub required: bool,
}

/// A loaded prompt with metadata and content
#[derive(Debug, Clone)]
pub struct Prompt {
    /// Name of the prompt (filename without .md extension)
    pub name: String,

    /// Metadata parsed from frontmatter
    pub metadata: PromptMetadata,

    /// Prompt content (body after frontmatter)
    pub content: String,

    /// Source location of the prompt file
    #[cfg_attr(not(test), allow(dead_code))]
    pub source: PromptSource,
}

/// Location where a prompt was loaded from
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq)]
pub enum PromptSource {
    /// Repo-specific prompt (.gru/prompts/)
    Repo(PathBuf),

    /// Built-in prompt (hardcoded)
    BuiltIn,

    /// Global prompt (~/.gru/prompts/)
    Global(PathBuf),
}

impl PromptSource {
    /// Returns a display string for the source
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn display(&self) -> String {
        match self {
            PromptSource::Repo(path) => format!("repo: {}", path.display()),
            PromptSource::BuiltIn => "built-in".to_string(),
            PromptSource::Global(path) => format!("global: {}", path.display()),
        }
    }
}

/// Parses a markdown file with YAML frontmatter
///
/// Expected format:
/// ```markdown
/// ---
/// description: Short description
/// requires: [issue]
/// params:
///   - name: target
///     description: What to target
///     required: false
/// ---
/// Prompt content here
/// ```
#[cfg_attr(not(test), allow(dead_code))]
fn parse_frontmatter(content: &str) -> Result<(PromptMetadata, String)> {
    let lines: Vec<&str> = content.lines().collect();

    // Check if file starts with frontmatter delimiter
    if lines.is_empty() || lines[0].trim() != "---" {
        // No frontmatter, return empty metadata and full content
        return Ok((
            PromptMetadata {
                description: None,
                requires: vec![],
                params: vec![],
            },
            content.to_string(),
        ));
    }

    // Find the closing delimiter
    let mut end_index = None;
    for (i, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            end_index = Some(i);
            break;
        }
    }

    let end_index = match end_index {
        Some(idx) => idx,
        None => bail!("Unclosed YAML frontmatter (missing closing '---')"),
    };

    // Extract and parse YAML
    let yaml_content = lines[1..end_index].join("\n");
    let metadata: PromptMetadata =
        serde_yml::from_str(&yaml_content).context("Failed to parse YAML frontmatter")?;

    // Extract prompt content (everything after closing delimiter)
    let prompt_content = if end_index + 1 < lines.len() {
        lines[end_index + 1..].join("\n").trim().to_string()
    } else {
        String::new()
    };

    Ok((metadata, prompt_content))
}

/// Loads a single prompt file from disk
fn load_prompt_file(path: &Path, name: &str, source: PromptSource) -> Result<Prompt> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read prompt file: {}", path.display()))?;

    let (metadata, prompt_content) = parse_frontmatter(&content)?;

    Ok(Prompt {
        name: name.to_string(),
        metadata,
        content: prompt_content,
        source,
    })
}

/// Scans a directory for .md prompt files
fn scan_prompt_directory(dir: &Path) -> Result<HashMap<String, PathBuf>> {
    let mut prompts = HashMap::new();

    if !dir.exists() {
        // Directory doesn't exist, return empty map
        return Ok(prompts);
    }

    let entries = fs::read_dir(dir)
        .with_context(|| format!("Failed to read prompt directory: {}", dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Only process .md files
        if path.extension().and_then(|s| s.to_str()) == Some("md") {
            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                prompts.insert(name.to_string(), path);
            }
        }
    }

    Ok(prompts)
}

/// Loads prompts with proper resolution priority: repo → built-in → global
///
/// **Priority order** (higher priority overrides lower priority):
/// 1. Repo-specific: `.gru/prompts/<name>.md` (highest priority)
/// 2. Built-in prompts: hardcoded defaults
/// 3. Global: `~/.gru/prompts/<name>.md` (lowest priority)
///
/// **Implementation note**: Loads in reverse order (global → built-in → repo)
/// where later entries override earlier ones in the HashMap. This achieves the
/// correct priority while being efficient (no need to check existence before insert).
///
/// Reserved system commands are validated separately and never loaded.
#[cfg_attr(not(test), allow(dead_code))]
pub fn load_prompts(repo_root: Option<&Path>) -> Result<HashMap<String, Prompt>> {
    load_prompts_internal(repo_root, dirs::home_dir().as_deref())
}

/// Internal function for loading prompts with explicit global directory path.
/// Used by public `load_prompts()` and for testing.
#[cfg_attr(not(test), allow(dead_code))]
fn load_prompts_internal(
    repo_root: Option<&Path>,
    global_root: Option<&Path>,
) -> Result<HashMap<String, Prompt>> {
    let mut prompts = HashMap::new();

    // 1. Load global prompts (~/.gru/prompts/)
    if let Some(global_root) = global_root {
        let global_dir = global_root.join(".gru").join("prompts");
        let global_files = scan_prompt_directory(&global_dir)?;

        for (name, path) in global_files {
            // Validate against reserved commands
            if let Err(e) = reserved_commands::validate_not_reserved(&name) {
                log::warn!("Warning: Skipping global prompt '{}': {}", name, e);
                continue;
            }

            match load_prompt_file(&path, &name, PromptSource::Global(path.clone())) {
                Ok(prompt) => {
                    prompts.insert(name, prompt);
                }
                Err(e) => {
                    log::warn!("Warning: Failed to load global prompt '{}': {}", name, e);
                }
            }
        }
    }

    // 2. Built-in prompts (currently none, will be added in Phase 4)
    // For now, this is a placeholder for future built-in prompts

    // 3. Load repo-specific prompts (.gru/prompts/) - these override global/built-in
    if let Some(repo_root) = repo_root {
        let repo_dir = repo_root.join(".gru").join("prompts");
        let repo_files = scan_prompt_directory(&repo_dir)?;

        for (name, path) in repo_files {
            // Validate against reserved commands
            if let Err(e) = reserved_commands::validate_not_reserved(&name) {
                log::warn!("Warning: Skipping repo prompt '{}': {}", name, e);
                continue;
            }

            match load_prompt_file(&path, &name, PromptSource::Repo(path.clone())) {
                Ok(prompt) => {
                    // This will override any global or built-in prompt with the same name
                    prompts.insert(name, prompt);
                }
                Err(e) => {
                    log::warn!("Warning: Failed to load repo prompt '{}': {}", name, e);
                }
            }
        }
    }

    Ok(prompts)
}

/// Validates that all required parameters declared in a prompt's frontmatter are provided
///
/// Returns a helpful error message listing all missing required parameters with their
/// descriptions (if available).
///
/// # Arguments
/// * `metadata` - The prompt metadata containing parameter declarations
/// * `provided` - The parameters provided via `--param` flags
///
/// # Example
/// ```ignore
/// let metadata = PromptMetadata {
///     params: vec![PromptParam { name: "component".into(), description: Some("Component to analyze".into()), required: true }],
///     ..
/// };
/// let mut provided = HashMap::new();
/// // Missing "component" param → returns error
/// validate_required_params(&metadata, &provided)?;
/// ```
#[cfg_attr(not(test), allow(dead_code))]
pub fn validate_required_params(
    metadata: &PromptMetadata,
    provided: &HashMap<String, String>,
) -> Result<()> {
    let missing: Vec<&PromptParam> = metadata
        .params
        .iter()
        .filter(|p| p.required && provided.get(&p.name).map_or(true, |v| v.trim().is_empty()))
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    let mut msg = String::from("Missing or empty required parameter(s):\n");
    for param in &missing {
        msg.push_str(&format!("  --param {}=<value>", param.name));
        if let Some(ref desc) = param.description {
            msg.push_str(&format!("  ({})", desc));
        }
        msg.push('\n');
    }
    msg.push_str("\nProvide the missing parameter(s) with --param KEY=<value>");

    bail!("{}", msg.trim())
}

/// Lists all prompts grouped by source (built-in, repo, global).
///
/// Unlike `load_prompts()` which merges by priority, this function returns
/// prompts in separate collections for display in `gru prompts`.
pub fn list_prompts_by_source(repo_root: Option<&Path>) -> Result<PromptsBySource> {
    list_prompts_by_source_internal(repo_root, dirs::home_dir().as_deref())
}

/// Internal function for listing prompts by source with explicit global directory path.
/// Used by public `list_prompts_by_source()` and for testing with controlled paths.
pub(crate) fn list_prompts_by_source_internal(
    repo_root: Option<&Path>,
    global_root: Option<&Path>,
) -> Result<PromptsBySource> {
    // Built-in prompts
    let built_in: Vec<(String, String)> = BUILT_IN_PROMPTS
        .iter()
        .map(|(name, desc)| (name.to_string(), desc.to_string()))
        .collect();

    // Repo prompts
    let mut repo_prompts = Vec::new();
    if let Some(repo_root) = repo_root {
        let repo_dir = repo_root.join(".gru").join("prompts");
        let repo_files = scan_prompt_directory(&repo_dir)?;

        let mut sorted_files: Vec<_> = repo_files.into_iter().collect();
        sorted_files.sort_by(|a, b| a.0.cmp(&b.0));

        for (name, path) in sorted_files {
            if reserved_commands::is_reserved(&name) {
                continue;
            }
            match load_prompt_file(&path, &name, PromptSource::Repo(path.clone())) {
                Ok(prompt) => repo_prompts.push(prompt),
                Err(e) => log::warn!("Warning: Failed to load repo prompt '{}': {}", name, e),
            }
        }
    }

    // Global prompts
    let mut global_prompts = Vec::new();
    if let Some(global_root) = global_root {
        let global_dir = global_root.join(".gru").join("prompts");
        let global_files = scan_prompt_directory(&global_dir)?;

        let mut sorted_files: Vec<_> = global_files.into_iter().collect();
        sorted_files.sort_by(|a, b| a.0.cmp(&b.0));

        for (name, path) in sorted_files {
            if reserved_commands::is_reserved(&name) {
                continue;
            }
            match load_prompt_file(&path, &name, PromptSource::Global(path.clone())) {
                Ok(prompt) => global_prompts.push(prompt),
                Err(e) => log::warn!("Warning: Failed to load global prompt '{}': {}", name, e),
            }
        }
    }

    Ok(PromptsBySource {
        built_in,
        repo: repo_prompts,
        global: global_prompts,
    })
}

/// Known context requirements for the `requires` field
const KNOWN_REQUIREMENTS: &[&str] = &["issue", "pr"];

/// Validates that context requirements declared in frontmatter are satisfied
///
/// Checks the `requires` field against the provided CLI flags.
/// Known requirements: `issue` (needs `--issue`), `pr` (needs `--pr`).
/// Unknown requirement names produce a warning but do not cause an error.
///
/// # Arguments
/// * `requires` - The list of context requirements from frontmatter
/// * `issue_provided` - Whether `--issue` was provided
/// * `pr_provided` - Whether `--pr` was provided
///
/// # Returns
/// A list of (requirement_name, flag_hint) pairs for any missing requirements
pub fn validate_requires(
    requires: &[String],
    issue_provided: bool,
    pr_provided: bool,
) -> Vec<(String, String)> {
    let mut missing = Vec::new();
    for req in requires {
        match req.as_str() {
            "issue" => {
                if !issue_provided {
                    missing.push(("issue".to_string(), "--issue <number>".to_string()));
                }
            }
            "pr" => {
                if !pr_provided {
                    missing.push(("pr".to_string(), "--pr <number>".to_string()));
                }
            }
            other => {
                log::warn!(
                    "Unknown requirement '{}' in prompt frontmatter (known: {:?})",
                    other,
                    KNOWN_REQUIREMENTS
                );
            }
        }
    }
    missing
}

/// Validates all prompt requirements (context + params) and returns a combined error
///
/// This is the main validation entry point. It checks both `requires` (context like
/// `--issue`, `--pr`) and `params` (custom `--param` values) and reports ALL missing
/// requirements in a single error message.
///
/// # Arguments
/// * `prompt_name` - Name of the prompt (for error messages)
/// * `metadata` - The prompt metadata containing requirements
/// * `issue_provided` - Whether `--issue` was provided
/// * `pr_provided` - Whether `--pr` was provided
/// * `provided_params` - The parameters provided via `--param` flags
#[cfg_attr(not(test), allow(dead_code))]
pub fn validate_prompt_requirements(
    prompt_name: &str,
    metadata: &PromptMetadata,
    issue_provided: bool,
    pr_provided: bool,
    provided_params: &HashMap<String, String>,
) -> Result<()> {
    let missing_requires = validate_requires(&metadata.requires, issue_provided, pr_provided);

    let missing_params: Vec<&PromptParam> = metadata
        .params
        .iter()
        .filter(|p| {
            p.required
                && provided_params
                    .get(&p.name)
                    .map_or(true, |v| v.trim().is_empty())
        })
        .collect();

    if missing_requires.is_empty() && missing_params.is_empty() {
        return Ok(());
    }

    let mut msg = format!(
        "Missing required parameters for prompt '{}':\n",
        prompt_name
    );

    for (_req, hint) in &missing_requires {
        msg.push_str(&format!("  {}\n", hint));
    }

    for param in &missing_params {
        msg.push_str(&format!("  --param {}=<value>", param.name));
        if let Some(ref desc) = param.description {
            msg.push_str(&format!("  ({})", desc));
        }
        msg.push('\n');
    }

    bail!("{}", msg.trim())
}

/// Validates a prompt's syntax
///
/// Currently checks:
/// - Prompt content is not empty
/// - Parameter names are valid identifiers
#[cfg_attr(not(test), allow(dead_code))]
pub fn validate_prompt(prompt: &Prompt) -> Result<()> {
    // Check content is not empty
    if prompt.content.trim().is_empty() {
        bail!("Prompt '{}' has empty content", prompt.name);
    }

    // Validate parameter names
    for param in &prompt.metadata.params {
        if param.name.is_empty() {
            bail!("Prompt '{}' has parameter with empty name", prompt.name);
        }

        // Check parameter name is a valid identifier (alphanumeric + underscore + hyphen)
        if !param
            .name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            bail!(
                "Prompt '{}' has invalid parameter name '{}' (must be alphanumeric, underscore, or hyphen)",
                prompt.name,
                param.name
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_parse_frontmatter_with_metadata() {
        let content = r#"---
description: Test prompt
requires: [issue]
params:
  - name: target
    description: Target file
    required: false
---
This is the prompt content"#;

        let (metadata, prompt_content) = parse_frontmatter(content).unwrap();

        assert_eq!(metadata.description, Some("Test prompt".to_string()));
        assert_eq!(metadata.requires, vec!["issue"]);
        assert_eq!(metadata.params.len(), 1);
        assert_eq!(metadata.params[0].name, "target");
        assert_eq!(
            metadata.params[0].description,
            Some("Target file".to_string())
        );
        assert!(!metadata.params[0].required);
        assert_eq!(prompt_content, "This is the prompt content");
    }

    #[test]
    fn test_parse_frontmatter_without_metadata() {
        let content = "Just prompt content without frontmatter";

        let (metadata, prompt_content) = parse_frontmatter(content).unwrap();

        assert_eq!(metadata.description, None);
        assert!(metadata.requires.is_empty());
        assert!(metadata.params.is_empty());
        assert_eq!(prompt_content, content);
    }

    #[test]
    fn test_parse_frontmatter_minimal() {
        let content = r#"---
description: Simple prompt
---
Content here"#;

        let (metadata, prompt_content) = parse_frontmatter(content).unwrap();

        assert_eq!(metadata.description, Some("Simple prompt".to_string()));
        assert!(metadata.requires.is_empty());
        assert!(metadata.params.is_empty());
        assert_eq!(prompt_content, "Content here");
    }

    #[test]
    fn test_parse_frontmatter_unclosed() {
        let content = r#"---
description: Broken prompt
This is missing the closing delimiter"#;

        let result = parse_frontmatter(content);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unclosed YAML frontmatter"));
    }

    #[test]
    fn test_validate_prompt_success() {
        let prompt = Prompt {
            name: "test".to_string(),
            metadata: PromptMetadata {
                description: Some("Test".to_string()),
                requires: vec![],
                params: vec![PromptParam {
                    name: "valid-name_123".to_string(),
                    description: None,
                    required: false,
                }],
            },
            content: "Prompt content".to_string(),
            source: PromptSource::BuiltIn,
        };

        assert!(validate_prompt(&prompt).is_ok());
    }

    #[test]
    fn test_validate_prompt_empty_content() {
        let prompt = Prompt {
            name: "test".to_string(),
            metadata: PromptMetadata {
                description: None,
                requires: vec![],
                params: vec![],
            },
            content: "   ".to_string(),
            source: PromptSource::BuiltIn,
        };

        let result = validate_prompt(&prompt);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty content"));
    }

    #[test]
    fn test_validate_prompt_invalid_param_name() {
        let prompt = Prompt {
            name: "test".to_string(),
            metadata: PromptMetadata {
                description: None,
                requires: vec![],
                params: vec![PromptParam {
                    name: "invalid@name!".to_string(),
                    description: None,
                    required: false,
                }],
            },
            content: "Content".to_string(),
            source: PromptSource::BuiltIn,
        };

        let result = validate_prompt(&prompt);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid parameter"));
    }

    #[test]
    fn test_load_prompts_from_directory() {
        let temp_dir = TempDir::new().unwrap();
        let prompts_dir = temp_dir.path().join(".gru").join("prompts");
        fs::create_dir_all(&prompts_dir).unwrap();

        // Create a valid prompt file
        let prompt_file = prompts_dir.join("test.md");
        fs::write(
            &prompt_file,
            r#"---
description: Test prompt
---
Test content"#,
        )
        .unwrap();

        let prompts = load_prompts(Some(temp_dir.path())).unwrap();

        assert_eq!(prompts.len(), 1);
        assert!(prompts.contains_key("test"));
        let prompt = &prompts["test"];
        assert_eq!(prompt.name, "test");
        assert_eq!(prompt.metadata.description, Some("Test prompt".to_string()));
        assert_eq!(prompt.content, "Test content");
    }

    #[test]
    fn test_load_prompts_reserved_name_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let prompts_dir = temp_dir.path().join(".gru").join("prompts");
        fs::create_dir_all(&prompts_dir).unwrap();

        // Try to create a prompt with a reserved name
        let prompt_file = prompts_dir.join("status.md");
        fs::write(
            &prompt_file,
            r#"---
description: Should be rejected
---
Content"#,
        )
        .unwrap();

        let prompts = load_prompts(Some(temp_dir.path())).unwrap();

        // Should be empty because 'status' is reserved
        assert_eq!(prompts.len(), 0);
    }

    #[test]
    fn test_repo_overrides_global() {
        let temp_dir = TempDir::new().unwrap();

        // Create global prompt
        let global_root = temp_dir.path().join("global");
        let global_dir = global_root.join(".gru").join("prompts");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(
            global_dir.join("test.md"),
            r#"---
description: Global prompt
---
Global content"#,
        )
        .unwrap();

        // Create repo prompt with same name
        let repo_root = temp_dir.path().join("repo");
        let repo_dir = repo_root.join(".gru").join("prompts");
        fs::create_dir_all(&repo_dir).unwrap();
        fs::write(
            repo_dir.join("test.md"),
            r#"---
description: Repo prompt
---
Repo content"#,
        )
        .unwrap();

        // Test that repo prompt overrides global prompt
        let prompts = load_prompts_internal(Some(&repo_root), Some(&global_root)).unwrap();

        assert_eq!(prompts.len(), 1);
        let prompt = &prompts["test"];
        assert_eq!(prompt.metadata.description, Some("Repo prompt".to_string()));
        assert_eq!(prompt.content, "Repo content");
        // Verify the source is from repo, not global
        assert!(matches!(prompt.source, PromptSource::Repo(_)));
    }

    #[test]
    fn test_scan_prompt_directory_missing_dir() {
        let temp_dir = TempDir::new().unwrap();
        let missing_dir = temp_dir.path().join("does-not-exist");

        let prompts = scan_prompt_directory(&missing_dir).unwrap();
        assert!(prompts.is_empty());
    }

    #[test]
    fn test_prompt_source_display() {
        let repo_source = PromptSource::Repo(PathBuf::from("/repo/.gru/prompts/fix.md"));
        assert!(repo_source.display().contains("repo:"));

        let builtin_source = PromptSource::BuiltIn;
        assert_eq!(builtin_source.display(), "built-in");

        let global_source = PromptSource::Global(PathBuf::from("~/.gru/prompts/fix.md"));
        assert!(global_source.display().contains("global:"));
    }

    #[test]
    fn test_validate_required_params_all_provided() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![
                PromptParam {
                    name: "component".to_string(),
                    description: Some("Component to analyze".to_string()),
                    required: true,
                },
                PromptParam {
                    name: "depth".to_string(),
                    description: None,
                    required: true,
                },
            ],
        };

        let mut provided = HashMap::new();
        provided.insert("component".to_string(), "auth".to_string());
        provided.insert("depth".to_string(), "3".to_string());

        assert!(validate_required_params(&metadata, &provided).is_ok());
    }

    #[test]
    fn test_validate_required_params_missing_one() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![
                PromptParam {
                    name: "component".to_string(),
                    description: Some("Component to analyze".to_string()),
                    required: true,
                },
                PromptParam {
                    name: "depth".to_string(),
                    description: None,
                    required: true,
                },
            ],
        };

        let mut provided = HashMap::new();
        provided.insert("component".to_string(), "auth".to_string());
        // Missing "depth"

        let result = validate_required_params(&metadata, &provided);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("depth"));
        assert!(err.contains("--param"));
    }

    #[test]
    fn test_validate_required_params_missing_with_description() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![PromptParam {
                name: "target".to_string(),
                description: Some("File or directory to focus on".to_string()),
                required: true,
            }],
        };

        let provided = HashMap::new();

        let result = validate_required_params(&metadata, &provided);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("target"));
        assert!(err.contains("File or directory to focus on"));
        assert!(err.contains("--param target=<value>"));
    }

    #[test]
    fn test_validate_required_params_optional_not_required() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![
                PromptParam {
                    name: "required_param".to_string(),
                    description: None,
                    required: true,
                },
                PromptParam {
                    name: "optional_param".to_string(),
                    description: None,
                    required: false,
                },
            ],
        };

        let mut provided = HashMap::new();
        provided.insert("required_param".to_string(), "value".to_string());
        // Not providing optional_param is fine

        assert!(validate_required_params(&metadata, &provided).is_ok());
    }

    #[test]
    fn test_validate_required_params_no_params_declared() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![],
        };

        let provided = HashMap::new();
        assert!(validate_required_params(&metadata, &provided).is_ok());
    }

    #[test]
    fn test_validate_required_params_extra_params_ok() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![PromptParam {
                name: "component".to_string(),
                description: None,
                required: true,
            }],
        };

        let mut provided = HashMap::new();
        provided.insert("component".to_string(), "auth".to_string());
        provided.insert("extra".to_string(), "ignored".to_string());

        assert!(validate_required_params(&metadata, &provided).is_ok());
    }

    #[test]
    fn test_validate_required_params_multiple_missing() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![
                PromptParam {
                    name: "a".to_string(),
                    description: Some("First param".to_string()),
                    required: true,
                },
                PromptParam {
                    name: "b".to_string(),
                    description: None,
                    required: true,
                },
                PromptParam {
                    name: "c".to_string(),
                    description: Some("Third param".to_string()),
                    required: true,
                },
            ],
        };

        let provided = HashMap::new();

        let result = validate_required_params(&metadata, &provided);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // All three should be listed
        assert!(err.contains("--param a=<value>"));
        assert!(err.contains("--param b=<value>"));
        assert!(err.contains("--param c=<value>"));
        assert!(err.contains("First param"));
        assert!(err.contains("Third param"));
    }

    #[test]
    fn test_validate_required_params_empty_value_rejected() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![PromptParam {
                name: "component".to_string(),
                description: None,
                required: true,
            }],
        };

        let mut provided = HashMap::new();
        provided.insert("component".to_string(), String::new());

        let result = validate_required_params(&metadata, &provided);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("component"));
    }

    #[test]
    fn test_validate_required_params_whitespace_only_rejected() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![PromptParam {
                name: "component".to_string(),
                description: None,
                required: true,
            }],
        };

        let mut provided = HashMap::new();
        provided.insert("component".to_string(), "   ".to_string());

        let result = validate_required_params(&metadata, &provided);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("component"));
    }

    #[test]
    fn test_validate_required_params_all_optional_none_provided() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![
                PromptParam {
                    name: "opt1".to_string(),
                    description: None,
                    required: false,
                },
                PromptParam {
                    name: "opt2".to_string(),
                    description: None,
                    required: false,
                },
            ],
        };

        assert!(validate_required_params(&metadata, &HashMap::new()).is_ok());
    }

    // --- validate_requires tests ---

    #[test]
    fn test_validate_requires_issue_provided() {
        let requires = vec!["issue".to_string()];
        let missing = validate_requires(&requires, true, false);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_validate_requires_issue_missing() {
        let requires = vec!["issue".to_string()];
        let missing = validate_requires(&requires, false, false);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, "issue");
        assert!(missing[0].1.contains("--issue"));
    }

    #[test]
    fn test_validate_requires_pr_provided() {
        let requires = vec!["pr".to_string()];
        let missing = validate_requires(&requires, false, true);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_validate_requires_pr_missing() {
        let requires = vec!["pr".to_string()];
        let missing = validate_requires(&requires, false, false);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, "pr");
        assert!(missing[0].1.contains("--pr"));
    }

    #[test]
    fn test_validate_requires_both_missing() {
        let requires = vec!["issue".to_string(), "pr".to_string()];
        let missing = validate_requires(&requires, false, false);
        assert_eq!(missing.len(), 2);
    }

    #[test]
    fn test_validate_requires_both_provided() {
        let requires = vec!["issue".to_string(), "pr".to_string()];
        let missing = validate_requires(&requires, true, true);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_validate_requires_empty() {
        let requires: Vec<String> = vec![];
        let missing = validate_requires(&requires, false, false);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_validate_requires_unknown_ignored() {
        let requires = vec!["unknown_thing".to_string()];
        let missing = validate_requires(&requires, false, false);
        // Unknown requirements don't produce missing entries (just a warning)
        assert!(missing.is_empty());
    }

    // --- validate_prompt_requirements tests ---

    #[test]
    fn test_validate_prompt_requirements_all_satisfied() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec!["issue".to_string()],
            params: vec![PromptParam {
                name: "component".to_string(),
                description: None,
                required: true,
            }],
        };

        let mut params = HashMap::new();
        params.insert("component".to_string(), "auth".to_string());

        let result = validate_prompt_requirements("fix", &metadata, true, false, &params);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_prompt_requirements_missing_requires() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec!["issue".to_string()],
            params: vec![],
        };

        let result = validate_prompt_requirements("fix", &metadata, false, false, &HashMap::new());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("fix"));
        assert!(err.contains("--issue"));
    }

    #[test]
    fn test_validate_prompt_requirements_missing_param() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![PromptParam {
                name: "target".to_string(),
                description: Some("File to target".to_string()),
                required: true,
            }],
        };

        let result =
            validate_prompt_requirements("analyze", &metadata, false, false, &HashMap::new());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("analyze"));
        assert!(err.contains("--param target=<value>"));
        assert!(err.contains("File to target"));
    }

    #[test]
    fn test_validate_prompt_requirements_combined_errors() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec!["issue".to_string()],
            params: vec![PromptParam {
                name: "component".to_string(),
                description: Some("Component name".to_string()),
                required: true,
            }],
        };

        let result =
            validate_prompt_requirements("analyze", &metadata, false, false, &HashMap::new());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // Both missing context and missing param should be in the same error
        assert!(
            err.contains("--issue"),
            "Error should mention --issue: {err}"
        );
        assert!(
            err.contains("--param component=<value>"),
            "Error should mention --param: {err}"
        );
        assert!(
            err.contains("analyze"),
            "Error should include prompt name: {err}"
        );
    }

    #[test]
    fn test_validate_prompt_requirements_no_requirements() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![],
        };

        let result =
            validate_prompt_requirements("simple", &metadata, false, false, &HashMap::new());
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_prompt_requirements_optional_params_ok() {
        let metadata = PromptMetadata {
            description: None,
            requires: vec![],
            params: vec![PromptParam {
                name: "optional".to_string(),
                description: None,
                required: false,
            }],
        };

        let result = validate_prompt_requirements("test", &metadata, false, false, &HashMap::new());
        assert!(result.is_ok());
    }
}

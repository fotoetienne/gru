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

/// Built-in prompt definitions: (name, description, content, requires)
///
/// These are the default prompts that ship with Gru. Repo-local prompts in
/// `.gru/prompts/` take precedence over these built-ins when they use the
/// same name.
///
/// Global prompts in `~/.gru/prompts/` are loaded before built-ins. If a
/// built-in with the same name has non-empty `content`, it is inserted and
/// will shadow the global prompt. A global prompt therefore only takes effect
/// when there is no corresponding built-in with content for that name.
pub const BUILT_IN_PROMPTS: &[BuiltInPrompt] = &[
    BuiltInPrompt {
        name: "fix",
        description: "Fix a GitHub issue with tests and PR",
        requires: &["issue"],
        content: r#"# Issue #{{ issue_number }}: {{ issue_title }}

URL: https://github.com/{{ repo_owner }}/{{ repo_name }}/issues/{{ issue_number }}
{{ labels }}

## Description:
{{ issue_body }}

# Instructions

## 1. Check if Decomposition is Needed
- Assess the issue's complexity:
  - Does it involve multiple distinct components or systems?
  - Does it have multiple acceptance criteria?
  - Would it take more than a few hours to complete?
  - Does it mix different types of work (backend + frontend + docs)?

- **If the issue is complex and should be broken down:**
  - Recommend to the user: "This issue seems complex. Run `/decompose $ARGUMENTS` to break it into smaller sub-issues first."
  - Stop the fix workflow here - wait for user to decompose

- **If the issue is focused and ready to fix:**
  - Proceed to the next step

## 2. Plan the Fix
- Explore the codebase to understand the relevant code
- Create a detailed plan using TodoWrite with specific steps to fix the issue
- Consider tests that need to be added or updated

## 3. Implement the Fix
- Work through each todo item
- Write clean, minimal code changes
- Add or update tests as needed
- Check CLAUDE.md for project-specific build/test commands
- Run tests to verify the fix

## 4. Code Review
- Make a commit with the changes
- Use the Task tool with `subagent_type='code-reviewer'` to perform an autonomous code review
- The code-reviewer agent will analyze the changes for:
  - Code correctness and logic errors
  - Security vulnerabilities
  - Error handling gaps
  - Edge cases
  - Adherence to project conventions (check CLAUDE.md)
  - Test coverage
- Address any issues raised by the code-reviewer before proceeding
- If the review identifies significant problems, iterate on the implementation

## 5. Finish Your Work

When your implementation is complete and ready for human review:

1. **Commit your implementation changes** with a descriptive commit message
2. **Push the branch** to the remote repository
3. Write `PR_DESCRIPTION.md` in the root of the repository with this format:
   ```markdown
   ## Summary
   - Key change 1
   - Key change 2

   ## Test plan
   - How you tested this
   - Commands run: cargo test, just check, etc.

   ## Notes
   - Context reviewers should know
   - Follow-up work if any
   ```

**DO NOT commit PR_DESCRIPTION.md** - Gru will read this file locally from your worktree, use it to create the PR description, mark the PR ready, and then delete it automatically.

**IMPORTANT:** Only write `PR_DESCRIPTION.md` when work is truly complete and ready for human review. If work is still in progress, don't create this file - Gru will create a draft PR instead.

## 6. Iterate on Feedback
- Look at CI check results
- Address any issues raised by the CI checks
- Read review comments
- Determine which comments require changes. Sometimes reviewers are wrong!
- Make the necessary changes
- For any comments that you've determined don't require changes, acknowledge them
- Make a reply that addresses each comment and includes a summary of the changes made
- Repeat until the PR is ready to merge
"#,
    },
    BuiltInPrompt {
        name: "review",
        description: "Review and respond to PR comments",
        requires: &["pr"],
        content: r#"# PR #{{ pr_number }}: {{ pr_title }}

URL: https://github.com/{{ repo_owner }}/{{ repo_name }}/pull/{{ pr_number }}

## Description:
{{ pr_body }}

# Instructions

Review and respond to all comments and reviews on this pull request.

## 1. Fetch Comments and Reviews
- Use `gh pr view {{ pr_number }} --repo {{ repo_owner }}/{{ repo_name }}` to get PR details and status
- Use `gh api repos/{{ repo_owner }}/{{ repo_name }}/pulls/{{ pr_number }}/comments` to fetch all review comments
- Use `gh api repos/{{ repo_owner }}/{{ repo_name }}/issues/{{ pr_number }}/comments` to fetch all issue comments
- Use `gh pr checks {{ pr_number }} --repo {{ repo_owner }}/{{ repo_name }}` to check CI status

## 2. Review All Feedback
- Read through all comments and reviews
- Identify questions, concerns, or suggestions that need responses
- Group related comments together
- Check if any CI checks are failing

## 3. Address Feedback
- For each comment or review:
  - Determine if it requires a code change
  - If yes, make the change and note what was done
  - If no, prepare a thoughtful response explaining why
- For CI failures:
  - Investigate the failure
  - Fix the underlying issue
  - Verify the fix locally

## 4. Implement Changes
- Make code changes to address review feedback
- Run tests to verify changes don't break anything
- Check CLAUDE.md for project-specific build/test commands

## 5. Commit and Respond
- Commit changes with a descriptive message summarizing what was addressed
- Push changes to the branch
- Use `gh pr comment {{ pr_number }} --repo {{ repo_owner }}/{{ repo_name }} -b "response"` to post a summary of changes made
- Reply to individual review comments where appropriate

## 6. Verify
- Run `gh pr checks {{ pr_number }} --repo {{ repo_owner }}/{{ repo_name }}` to verify CI passes
- Confirm all review comments have been addressed or responded to
"#,
    },
    BuiltInPrompt {
        name: "rebase",
        description: "Rebase branch with intelligent conflict resolution",
        requires: &[],
        content: r#"Rebase the current working branch onto the default branch with intelligent conflict resolution.

Branch: {{ branch_name }}
Base branch: {{ base_branch }}
Worktree: {{ worktree_path }}

## Steps

### 1. Verify Git State
- Confirm you are on a feature branch (not the default branch)
- Ensure the working directory is clean (no uncommitted changes)
- If dirty, ask the user to commit or stash first

### 2. Fetch and Rebase
- Run `git fetch origin {{ base_branch }}` to get the latest upstream changes
- Run `git rebase origin/{{ base_branch }}` to start the rebase

### 3. Resolve Conflicts
If conflicts occur during the rebase:

**Automatically resolve these confidently:**
- Independent changes in different code sections
- Import additions (merge and sort them)
- Refactoring on the default branch (adapt your code)
- Both branches adding tests or config (merge both)

**Pause and report these to the user:**
- Logic conflicts with different approaches
- Security or permission changes
- Architectural decisions
- Configuration value conflicts
- Anything ambiguous or uncertain

For each conflict requiring input, provide:
- Clear explanation of the conflict
- Context from the PR/issue
- Why you are uncertain
- Resolution options

After resolving each conflict:
- Stage the resolved files with `git add <file>`
- Continue with `git rebase --continue`

### 4. After Rebase Completes
- Review the changes: `git log --oneline origin/{{ base_branch }}..HEAD`
- Run the project's test suite to ensure nothing broke (check CLAUDE.md for test commands)
- Report the result to the user

### 5. If Something Goes Wrong
- If the rebase cannot be completed, abort with `git rebase --abort`
- Report what went wrong and suggest next steps
"#,
    },
];

/// A built-in prompt definition compiled into the binary
#[derive(Debug)]
pub struct BuiltInPrompt {
    pub name: &'static str,
    pub description: &'static str,
    pub requires: &'static [&'static str],
    pub content: &'static str,
}

impl BuiltInPrompt {
    /// Converts this built-in definition into a `Prompt` struct.
    /// Returns `None` if the built-in has no content (placeholder for future implementation).
    pub fn to_prompt(&self) -> Option<Prompt> {
        if self.content.trim().is_empty() {
            return None;
        }
        Some(Prompt {
            name: self.name.to_string(),
            metadata: PromptMetadata {
                description: Some(self.description.to_string()),
                requires: self.requires.iter().map(|s| s.to_string()).collect(),
                params: vec![],
            },
            content: self.content.to_string(),
            source: PromptSource::BuiltIn,
        })
    }
}

/// Prompts grouped by their source, for display in `gru prompts`
pub struct PromptsBySource {
    pub built_in: Vec<(String, String)>,
    pub repo: Vec<Prompt>,
    pub global: Vec<Prompt>,
}

/// Metadata for a prompt file, parsed from YAML frontmatter
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
pub fn load_prompts(repo_root: Option<&Path>) -> Result<HashMap<String, Prompt>> {
    load_prompts_internal(repo_root, dirs::home_dir().as_deref())
}

/// Internal function for loading prompts with explicit global directory path.
/// Used by public `load_prompts()` and for testing.
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

    // 2. Built-in prompts (override global, overridden by repo)
    for builtin in BUILT_IN_PROMPTS {
        if let Some(prompt) = builtin.to_prompt() {
            prompts.insert(builtin.name.to_string(), prompt);
        }
    }

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

/// Resolves a prompt by name, checking repo overrides, built-in, and global prompts.
///
/// This is the main entry point for commands like `gru fix` that need to load
/// a built-in prompt while allowing user overrides.
///
/// **Performance note:** This loads all prompts from disk (scanning `.gru/prompts/`
/// directories) and then extracts the requested one. The cost is proportional to
/// the total number of prompt files, not O(1). This is acceptable since the number
/// of prompts is small, but could be optimized with a targeted lookup path if
/// prompt count grows significantly.
///
/// Returns `None` if no prompt with that name exists (neither built-in nor custom).
pub fn resolve_prompt(name: &str, repo_root: Option<&Path>) -> Result<Option<Prompt>> {
    let mut prompts = load_prompts(repo_root)?;
    Ok(prompts.remove(name))
}

/// Collects required parameters that are missing or have empty/whitespace-only values
fn collect_missing_params<'a>(
    metadata: &'a PromptMetadata,
    provided: &HashMap<String, String>,
) -> Vec<&'a PromptParam> {
    metadata
        .params
        .iter()
        .filter(|p| p.required && provided.get(&p.name).map_or(true, |v| v.trim().is_empty()))
        .collect()
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
    let missing = collect_missing_params(metadata, provided);

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
        .map(|b| (b.name.to_string(), b.description.to_string()))
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
        match req.trim().to_lowercase().as_str() {
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
            _ => {
                log::warn!(
                    "Unknown requirement '{}' in prompt frontmatter (known: {:?})",
                    req,
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
pub fn validate_prompt_requirements(
    prompt_name: &str,
    metadata: &PromptMetadata,
    issue_provided: bool,
    pr_provided: bool,
    provided_params: &HashMap<String, String>,
) -> Result<()> {
    let missing_requires = validate_requires(&metadata.requires, issue_provided, pr_provided);
    let missing_params = collect_missing_params(metadata, provided_params);

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

        // Should contain the custom "test" prompt plus any built-in prompts with content
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

        // 'status' is reserved and should be filtered out
        assert!(!prompts.contains_key("status"));
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

    #[test]
    fn test_validate_requires_case_insensitive() {
        // "Issue" and "PR" should be recognized (case-insensitive matching)
        let requires = vec!["Issue".to_string(), "PR".to_string()];
        let missing = validate_requires(&requires, false, false);
        assert_eq!(missing.len(), 2);

        // Providing them should satisfy the check
        let missing = validate_requires(&requires, true, true);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_validate_requires_trims_whitespace() {
        // Values with leading/trailing whitespace should still match
        let requires = vec!["issue ".to_string(), " pr".to_string()];
        let missing = validate_requires(&requires, false, false);
        assert_eq!(missing.len(), 2);

        let missing = validate_requires(&requires, true, true);
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

    // --- Built-in prompt tests ---

    #[test]
    fn test_builtin_fix_prompt_has_content() {
        let fix = BUILT_IN_PROMPTS.iter().find(|b| b.name == "fix").unwrap();
        assert!(!fix.content.trim().is_empty());
        assert_eq!(fix.description, "Fix a GitHub issue with tests and PR");
        assert_eq!(fix.requires, &["issue"]);
    }

    #[test]
    fn test_builtin_to_prompt_with_content() {
        let fix = BUILT_IN_PROMPTS.iter().find(|b| b.name == "fix").unwrap();
        let prompt = fix.to_prompt();
        assert!(prompt.is_some());

        let prompt = prompt.unwrap();
        assert_eq!(prompt.name, "fix");
        assert!(matches!(prompt.source, PromptSource::BuiltIn));
        assert_eq!(prompt.metadata.requires, vec!["issue"]);
        assert!(prompt.content.contains("{{ issue_number }}"));
        assert!(prompt.content.contains("{{ issue_title }}"));
        assert!(prompt.content.contains("{{ issue_body }}"));
    }

    #[test]
    fn test_builtin_review_prompt_has_content() {
        let review = BUILT_IN_PROMPTS
            .iter()
            .find(|b| b.name == "review")
            .unwrap();
        let prompt = review.to_prompt();
        assert!(prompt.is_some());

        let prompt = prompt.unwrap();
        assert_eq!(prompt.name, "review");
        assert!(matches!(prompt.source, PromptSource::BuiltIn));
        assert_eq!(prompt.metadata.requires, vec!["pr"]);
        assert!(prompt.content.contains("{{ pr_number }}"));
        assert!(prompt.content.contains("{{ pr_title }}"));
        assert!(prompt.content.contains("{{ pr_body }}"));
    }

    #[test]
    fn test_builtin_rebase_prompt_has_content() {
        let rebase = BUILT_IN_PROMPTS
            .iter()
            .find(|b| b.name == "rebase")
            .unwrap();
        let prompt = rebase.to_prompt();
        assert!(prompt.is_some());

        let prompt = prompt.unwrap();
        assert_eq!(prompt.name, "rebase");
        assert!(matches!(prompt.source, PromptSource::BuiltIn));
        assert!(prompt.metadata.requires.is_empty());
        assert!(prompt.content.contains("{{ branch_name }}"));
        assert!(prompt.content.contains("{{ base_branch }}"));
        assert!(prompt.content.contains("{{ worktree_path }}"));
    }

    #[test]
    fn test_builtin_fix_included_in_load_prompts() {
        let temp_dir = TempDir::new().unwrap();
        let prompts = load_prompts_internal(Some(temp_dir.path()), Some(temp_dir.path())).unwrap();

        assert!(prompts.contains_key("fix"));
        let fix = &prompts["fix"];
        assert!(matches!(fix.source, PromptSource::BuiltIn));
        assert!(fix
            .content
            .contains("## 1. Check if Decomposition is Needed"));
    }

    #[test]
    fn test_builtin_review_included_in_load_prompts() {
        let temp_dir = TempDir::new().unwrap();
        let prompts = load_prompts_internal(Some(temp_dir.path()), Some(temp_dir.path())).unwrap();

        assert!(prompts.contains_key("review"));
        let review = &prompts["review"];
        assert!(matches!(review.source, PromptSource::BuiltIn));
        assert!(review.content.contains("## 1. Fetch Comments and Reviews"));
    }

    #[test]
    fn test_builtin_rebase_included_in_load_prompts() {
        let temp_dir = TempDir::new().unwrap();
        let prompts = load_prompts_internal(Some(temp_dir.path()), Some(temp_dir.path())).unwrap();

        assert!(prompts.contains_key("rebase"));
        let rebase = &prompts["rebase"];
        assert!(matches!(rebase.source, PromptSource::BuiltIn));
        assert!(rebase.content.contains("## 1. Verify Git State"));
    }

    #[test]
    fn test_builtin_rebase_template_has_expected_variables() {
        let rebase = BUILT_IN_PROMPTS
            .iter()
            .find(|b| b.name == "rebase")
            .unwrap();
        let prompt = rebase.to_prompt().unwrap();

        // Template should reference git context variables
        assert!(prompt.content.contains("{{ branch_name }}"));
        assert!(prompt.content.contains("{{ base_branch }}"));
        assert!(prompt.content.contains("{{ worktree_path }}"));

        // Template should contain the key workflow steps
        assert!(prompt.content.contains("Verify Git State"));
        assert!(prompt.content.contains("Fetch and Rebase"));
        assert!(prompt.content.contains("Resolve Conflicts"));
    }

    #[test]
    fn test_builtin_rebase_no_requires() {
        let rebase = BUILT_IN_PROMPTS
            .iter()
            .find(|b| b.name == "rebase")
            .unwrap();
        assert!(
            rebase.requires.is_empty(),
            "rebase should have no requires (works on current branch)"
        );
        assert_eq!(
            rebase.description,
            "Rebase branch with intelligent conflict resolution"
        );
    }

    #[test]
    fn test_repo_prompt_overrides_builtin() {
        let temp_dir = TempDir::new().unwrap();
        let prompts_dir = temp_dir.path().join(".gru").join("prompts");
        fs::create_dir_all(&prompts_dir).unwrap();

        // Create a repo prompt that overrides the built-in "fix"
        fs::write(
            prompts_dir.join("fix.md"),
            r#"---
description: Custom fix workflow
requires: [issue]
---
Custom fix for issue #{{ issue_number }}"#,
        )
        .unwrap();

        let prompts = load_prompts_internal(Some(temp_dir.path()), Some(temp_dir.path())).unwrap();

        let fix = &prompts["fix"];
        assert!(matches!(fix.source, PromptSource::Repo(_)));
        assert_eq!(
            fix.metadata.description,
            Some("Custom fix workflow".to_string())
        );
        assert_eq!(fix.content, "Custom fix for issue #{{ issue_number }}");
    }

    #[test]
    fn test_resolve_prompt_finds_builtin() {
        let temp_dir = TempDir::new().unwrap();
        let prompt = resolve_prompt("fix", Some(temp_dir.path())).unwrap();
        assert!(prompt.is_some());

        let prompt = prompt.unwrap();
        assert_eq!(prompt.name, "fix");
        assert!(matches!(prompt.source, PromptSource::BuiltIn));
    }

    #[test]
    fn test_resolve_prompt_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let prompt = resolve_prompt("nonexistent", Some(temp_dir.path())).unwrap();
        assert!(prompt.is_none());
    }

    #[test]
    fn test_builtin_fix_template_has_expected_variables() {
        let fix = BUILT_IN_PROMPTS.iter().find(|b| b.name == "fix").unwrap();
        let prompt = fix.to_prompt().unwrap();

        // Template should reference standard context variables
        assert!(prompt.content.contains("{{ issue_number }}"));
        assert!(prompt.content.contains("{{ issue_title }}"));
        assert!(prompt.content.contains("{{ issue_body }}"));
        assert!(prompt.content.contains("{{ repo_owner }}"));
        assert!(prompt.content.contains("{{ repo_name }}"));
        assert!(prompt.content.contains("{{ labels }}"));

        // Template should contain the key workflow steps
        assert!(prompt.content.contains("Check if Decomposition is Needed"));
        assert!(prompt.content.contains("Plan the Fix"));
        assert!(prompt.content.contains("Implement the Fix"));
        assert!(prompt.content.contains("Code Review"));
        assert!(prompt.content.contains("Finish Your Work"));
        assert!(prompt.content.contains("Iterate on Feedback"));
    }
}

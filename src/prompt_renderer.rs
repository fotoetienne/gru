/// Module for rendering prompts with variable substitution
///
/// Implements Phase 2 of Custom Prompts feature (see plans/CUSTOM_PROMPTS_PRD.md)
///
/// Responsibilities:
/// - Define `PromptContext` struct with all standard template variables
/// - Implement simple `{{ variable }}` substitution (no Mustache logic yet)
/// - Handle missing variables gracefully (replace with empty string)
/// - Support whitespace insensitivity: `{{var}}` and `{{ var }}` both work
/// - Case-sensitive variable names
///
/// # Variable Name Format
///
/// Variable names must:
/// - Start with a letter (a-z, A-Z) or underscore (_)
/// - After the initial character, contain only alphanumeric characters (a-z, A-Z, 0-9),
///   underscores (_), or hyphens (-)
/// - Be case-sensitive (e.g., `Name` and `name` are different)
///
/// # Security Considerations
///
/// Variable values are substituted as-is without escaping. The security model
/// assumes that prompt templates and context values come from trusted sources:
///
/// - Templates are loaded from `.gru/prompts/` directories (controlled by developers)
/// - Standard context variables are populated from GitHub/git APIs (trusted sources)
/// - Custom params from `--param` flags are user-controlled but users are trusted
///
/// When integrating with external systems, callers should consider:
/// - Variable values may contain special characters (quotes, backticks, $, etc.)
/// - Rendered prompts passed to shells may need additional escaping
/// - Nested `{{ }}` patterns in values will NOT be re-processed (single-pass substitution)
///
/// # Custom Parameter Override Behavior
///
/// Custom params from `--param` flags override standard variables with the same name.
/// This is intentional to allow users to customize behavior, but means:
/// - `--param issue_number=999` will override the actual issue number
/// - No warning is issued when standard variables are shadowed
use regex::Regex;
use std::collections::HashMap;
use std::path::PathBuf;

use once_cell::sync::Lazy;

use crate::prompt_loader::Prompt;

/// Regex pattern to match template variables: {{ variable_name }}
/// Supports optional whitespace around variable names
/// Variable names can contain alphanumeric characters, underscores, and hyphens
#[cfg_attr(not(test), allow(dead_code))]
static VARIABLE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\{\{\s*([a-zA-Z_][a-zA-Z0-9_-]*)\s*\}\}").unwrap());

/// Context for rendering prompts with variable substitution
///
/// Contains all standard template variables available to prompts.
/// Variables that are not available are set to None and will be
/// replaced with empty strings during rendering.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    // GitHub context (when --issue or --pr provided)
    /// GitHub issue number
    pub issue_number: Option<u64>,
    /// GitHub issue title
    pub issue_title: Option<String>,
    /// GitHub issue body/description
    pub issue_body: Option<String>,
    /// GitHub PR number
    pub pr_number: Option<u64>,
    /// GitHub PR title
    pub pr_title: Option<String>,
    /// GitHub PR body/description
    pub pr_body: Option<String>,

    // Git context
    /// Path to the worktree directory
    pub worktree_path: Option<PathBuf>,
    /// Current git branch name
    pub branch_name: Option<String>,
    /// Base branch (e.g., main, master)
    pub base_branch: Option<String>,
    /// Repository owner (GitHub username or org)
    pub repo_owner: Option<String>,
    /// Repository name
    pub repo_name: Option<String>,

    // Environment
    /// Current working directory
    pub cwd: Option<PathBuf>,

    // Custom params from --param key=value
    /// Custom parameters provided via CLI
    pub params: HashMap<String, String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl PromptContext {
    /// Creates a new empty PromptContext
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a PromptContext with custom parameters
    pub fn with_params(params: HashMap<String, String>) -> Self {
        Self {
            params,
            ..Default::default()
        }
    }

    /// Converts the context to a HashMap of variable name -> value
    ///
    /// This flattens all context variables into a single map for easy lookup.
    /// None values are not included in the map (they will be replaced with empty strings).
    pub fn to_variables(&self) -> HashMap<String, String> {
        let mut vars = HashMap::new();

        // GitHub context
        if let Some(n) = self.issue_number {
            vars.insert("issue_number".to_string(), n.to_string());
        }
        if let Some(ref s) = self.issue_title {
            vars.insert("issue_title".to_string(), s.clone());
        }
        if let Some(ref s) = self.issue_body {
            vars.insert("issue_body".to_string(), s.clone());
        }
        if let Some(n) = self.pr_number {
            vars.insert("pr_number".to_string(), n.to_string());
        }
        if let Some(ref s) = self.pr_title {
            vars.insert("pr_title".to_string(), s.clone());
        }
        if let Some(ref s) = self.pr_body {
            vars.insert("pr_body".to_string(), s.clone());
        }

        // Git context
        if let Some(ref p) = self.worktree_path {
            vars.insert("worktree_path".to_string(), p.display().to_string());
        }
        if let Some(ref s) = self.branch_name {
            vars.insert("branch_name".to_string(), s.clone());
        }
        if let Some(ref s) = self.base_branch {
            vars.insert("base_branch".to_string(), s.clone());
        }
        if let Some(ref s) = self.repo_owner {
            vars.insert("repo_owner".to_string(), s.clone());
        }
        if let Some(ref s) = self.repo_name {
            vars.insert("repo_name".to_string(), s.clone());
        }

        // Environment
        if let Some(ref p) = self.cwd {
            vars.insert("cwd".to_string(), p.display().to_string());
        }

        // Custom params override standard variables
        for (key, value) in &self.params {
            vars.insert(key.clone(), value.clone());
        }

        vars
    }
}

/// Renders a template string with variable substitution
///
/// Replaces `{{ variable_name }}` patterns with values from the variables map.
/// Missing variables are replaced with empty strings.
///
/// # Arguments
/// * `template` - The template string with `{{ variable }}` placeholders
/// * `variables` - Map of variable names to their values
///
/// # Returns
/// The rendered string with all variables substituted
#[cfg_attr(not(test), allow(dead_code))]
pub fn render_template(template: &str, variables: &HashMap<String, String>) -> String {
    VARIABLE_PATTERN
        .replace_all(template, |caps: &regex::Captures| {
            let var_name = &caps[1];
            variables.get(var_name).cloned().unwrap_or_default()
        })
        .to_string()
}

/// Renders a prompt with the given context
///
/// # Arguments
/// * `prompt` - The loaded prompt with content to render
/// * `context` - The context containing variable values
///
/// # Returns
/// The rendered prompt content
#[cfg_attr(not(test), allow(dead_code))]
pub fn render_prompt(prompt: &Prompt, context: &PromptContext) -> String {
    let variables = context.to_variables();
    render_template(&prompt.content, &variables)
}

/// Extracts all variable names used in a template
///
/// Useful for validation and debugging to see what variables
/// a template expects.
///
/// # Arguments
/// * `template` - The template string to scan
///
/// # Returns
/// A vector of unique variable names found in the template
#[cfg_attr(not(test), allow(dead_code))]
pub fn extract_variables(template: &str) -> Vec<String> {
    use std::collections::HashSet;

    let vars: HashSet<String> = VARIABLE_PATTERN
        .captures_iter(template)
        .map(|caps| caps[1].to_string())
        .collect();
    let mut result: Vec<_> = vars.into_iter().collect();
    result.sort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_loader::{PromptMetadata, PromptSource};

    #[test]
    fn test_render_template_basic() {
        let template = "Hello {{ name }}!";
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "World".to_string());

        let result = render_template(template, &vars);
        assert_eq!(result, "Hello World!");
    }

    #[test]
    fn test_render_template_multiple_variables() {
        let template = "Issue #{{ issue_number }}: {{ issue_title }}";
        let mut vars = HashMap::new();
        vars.insert("issue_number".to_string(), "42".to_string());
        vars.insert("issue_title".to_string(), "Fix the bug".to_string());

        let result = render_template(template, &vars);
        assert_eq!(result, "Issue #42: Fix the bug");
    }

    #[test]
    fn test_render_template_missing_variable() {
        let template = "Hello {{ missing }}!";
        let vars = HashMap::new();

        let result = render_template(template, &vars);
        assert_eq!(result, "Hello !");
    }

    #[test]
    fn test_render_template_whitespace_insensitive() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "Test".to_string());

        // No whitespace
        assert_eq!(render_template("{{name}}", &vars), "Test");
        // Single space
        assert_eq!(render_template("{{ name }}", &vars), "Test");
        // Multiple spaces
        assert_eq!(render_template("{{  name  }}", &vars), "Test");
        // Tabs
        assert_eq!(render_template("{{\tname\t}}", &vars), "Test");
        // Mixed whitespace
        assert_eq!(render_template("{{ \t name \t }}", &vars), "Test");
    }

    #[test]
    fn test_render_template_case_sensitive() {
        let mut vars = HashMap::new();
        vars.insert("Name".to_string(), "Upper".to_string());
        vars.insert("name".to_string(), "Lower".to_string());

        assert_eq!(render_template("{{ Name }}", &vars), "Upper");
        assert_eq!(render_template("{{ name }}", &vars), "Lower");
    }

    #[test]
    fn test_render_template_with_underscores_and_hyphens() {
        let mut vars = HashMap::new();
        vars.insert("repo_owner".to_string(), "alice".to_string());
        vars.insert("base-branch".to_string(), "main".to_string());
        vars.insert("_private".to_string(), "secret".to_string());

        assert_eq!(render_template("{{ repo_owner }}", &vars), "alice");
        assert_eq!(render_template("{{ base-branch }}", &vars), "main");
        assert_eq!(render_template("{{ _private }}", &vars), "secret");
    }

    #[test]
    fn test_render_template_same_variable_multiple_times() {
        let template = "{{ x }} + {{ x }} = {{ result }}";
        let mut vars = HashMap::new();
        vars.insert("x".to_string(), "2".to_string());
        vars.insert("result".to_string(), "4".to_string());

        let result = render_template(template, &vars);
        assert_eq!(result, "2 + 2 = 4");
    }

    #[test]
    fn test_render_template_preserves_non_variable_text() {
        let template = "This is plain text with {{ var }} in the middle.";
        let mut vars = HashMap::new();
        vars.insert("var".to_string(), "a value".to_string());

        let result = render_template(template, &vars);
        assert_eq!(result, "This is plain text with a value in the middle.");
    }

    #[test]
    fn test_render_template_multiline() {
        let template = r#"# Issue #{{ issue_number }}: {{ issue_title }}

## Description
{{ issue_body }}

## Branch
{{ branch_name }}"#;

        let mut vars = HashMap::new();
        vars.insert("issue_number".to_string(), "123".to_string());
        vars.insert("issue_title".to_string(), "Add feature".to_string());
        vars.insert(
            "issue_body".to_string(),
            "Detailed description here.".to_string(),
        );
        vars.insert("branch_name".to_string(), "feature/add-thing".to_string());

        let result = render_template(template, &vars);
        let expected = r#"# Issue #123: Add feature

## Description
Detailed description here.

## Branch
feature/add-thing"#;

        assert_eq!(result, expected);
    }

    #[test]
    fn test_render_template_empty_value() {
        let template = "Before {{ empty }} After";
        let mut vars = HashMap::new();
        vars.insert("empty".to_string(), String::new());

        let result = render_template(template, &vars);
        assert_eq!(result, "Before  After");
    }

    #[test]
    fn test_render_template_no_variables() {
        let template = "Plain text without any variables.";
        let vars = HashMap::new();

        let result = render_template(template, &vars);
        assert_eq!(result, template);
    }

    #[test]
    fn test_render_template_invalid_syntax_preserved() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "Test".to_string());

        // Single braces should be preserved
        assert_eq!(render_template("{ name }", &vars), "{ name }");
        // Three braces should be preserved
        assert_eq!(render_template("{{{ name }}}", &vars), "{Test}");
        // Unclosed should be preserved
        assert_eq!(render_template("{{ name", &vars), "{{ name");
        // Empty braces should be preserved
        assert_eq!(render_template("{{ }}", &vars), "{{ }}");
    }

    #[test]
    fn test_prompt_context_to_variables() {
        let mut params = HashMap::new();
        params.insert("custom_key".to_string(), "custom_value".to_string());

        let context = PromptContext {
            issue_number: Some(42),
            issue_title: Some("Test Issue".to_string()),
            issue_body: Some("Issue description".to_string()),
            pr_number: Some(100),
            pr_title: Some("Test PR".to_string()),
            pr_body: Some("PR description".to_string()),
            worktree_path: Some(PathBuf::from("/path/to/worktree")),
            branch_name: Some("feature/test".to_string()),
            base_branch: Some("main".to_string()),
            repo_owner: Some("owner".to_string()),
            repo_name: Some("repo".to_string()),
            cwd: Some(PathBuf::from("/current/dir")),
            params,
        };

        let vars = context.to_variables();

        assert_eq!(vars.get("issue_number"), Some(&"42".to_string()));
        assert_eq!(vars.get("issue_title"), Some(&"Test Issue".to_string()));
        assert_eq!(
            vars.get("issue_body"),
            Some(&"Issue description".to_string())
        );
        assert_eq!(vars.get("pr_number"), Some(&"100".to_string()));
        assert_eq!(vars.get("pr_title"), Some(&"Test PR".to_string()));
        assert_eq!(vars.get("pr_body"), Some(&"PR description".to_string()));
        assert_eq!(
            vars.get("worktree_path"),
            Some(&"/path/to/worktree".to_string())
        );
        assert_eq!(vars.get("branch_name"), Some(&"feature/test".to_string()));
        assert_eq!(vars.get("base_branch"), Some(&"main".to_string()));
        assert_eq!(vars.get("repo_owner"), Some(&"owner".to_string()));
        assert_eq!(vars.get("repo_name"), Some(&"repo".to_string()));
        assert_eq!(vars.get("cwd"), Some(&"/current/dir".to_string()));
        assert_eq!(vars.get("custom_key"), Some(&"custom_value".to_string()));
    }

    #[test]
    fn test_prompt_context_with_none_values() {
        let context = PromptContext {
            issue_number: Some(42),
            issue_title: None,
            ..Default::default()
        };

        let vars = context.to_variables();

        assert_eq!(vars.get("issue_number"), Some(&"42".to_string()));
        assert!(!vars.contains_key("issue_title"));
        assert!(!vars.contains_key("pr_number"));
    }

    #[test]
    fn test_prompt_context_params_override() {
        let mut params = HashMap::new();
        // Custom param with same name as standard variable
        params.insert("issue_number".to_string(), "override".to_string());

        let context = PromptContext {
            issue_number: Some(42),
            params,
            ..Default::default()
        };

        let vars = context.to_variables();

        // Custom params should override standard variables
        assert_eq!(vars.get("issue_number"), Some(&"override".to_string()));
    }

    #[test]
    fn test_render_prompt() {
        let prompt = Prompt {
            name: "test".to_string(),
            metadata: PromptMetadata {
                description: Some("Test prompt".to_string()),
                requires: vec![],
                params: vec![],
            },
            content: "Fix issue #{{ issue_number }}: {{ issue_title }}".to_string(),
            source: PromptSource::BuiltIn,
        };

        let context = PromptContext {
            issue_number: Some(42),
            issue_title: Some("Bug fix".to_string()),
            ..Default::default()
        };

        let result = render_prompt(&prompt, &context);
        assert_eq!(result, "Fix issue #42: Bug fix");
    }

    #[test]
    fn test_extract_variables() {
        let template = "{{ a }} and {{ b }} and {{ a }} again";
        let vars = extract_variables(template);

        assert_eq!(vars.len(), 2);
        assert!(vars.contains(&"a".to_string()));
        assert!(vars.contains(&"b".to_string()));
    }

    #[test]
    fn test_extract_variables_empty() {
        let template = "No variables here";
        let vars = extract_variables(template);
        assert!(vars.is_empty());
    }

    #[test]
    fn test_extract_variables_complex() {
        let template = r#"# Issue #{{ issue_number }}: {{ issue_title }}

Working in {{ worktree_path }} on branch {{ branch_name }}.
Targeting {{ base_branch }} in {{ repo_owner }}/{{ repo_name }}."#;

        let vars = extract_variables(template);

        assert_eq!(vars.len(), 7);
        assert!(vars.contains(&"issue_number".to_string()));
        assert!(vars.contains(&"issue_title".to_string()));
        assert!(vars.contains(&"worktree_path".to_string()));
        assert!(vars.contains(&"branch_name".to_string()));
        assert!(vars.contains(&"base_branch".to_string()));
        assert!(vars.contains(&"repo_owner".to_string()));
        assert!(vars.contains(&"repo_name".to_string()));
    }

    #[test]
    fn test_variable_name_validation() {
        let mut vars = HashMap::new();
        vars.insert("valid".to_string(), "yes".to_string());

        // Valid variable names start with letter or underscore
        assert_eq!(render_template("{{ valid }}", &vars), "yes");
        assert_eq!(render_template("{{ _valid }}", &vars), ""); // _valid not in vars

        // Numbers at start are invalid (regex won't match)
        assert_eq!(
            render_template("{{ 123invalid }}", &vars),
            "{{ 123invalid }}"
        );

        // Special characters are invalid (regex won't match)
        assert_eq!(render_template("{{ invalid! }}", &vars), "{{ invalid! }}");
        assert_eq!(
            render_template("{{ invalid@name }}", &vars),
            "{{ invalid@name }}"
        );
    }

    #[test]
    fn test_prompt_context_new() {
        let context = PromptContext::new();
        assert!(context.issue_number.is_none());
        assert!(context.params.is_empty());
    }

    #[test]
    fn test_prompt_context_with_params() {
        let mut params = HashMap::new();
        params.insert("key".to_string(), "value".to_string());

        let context = PromptContext::with_params(params);
        assert_eq!(context.params.get("key"), Some(&"value".to_string()));
        assert!(context.issue_number.is_none());
    }

    #[test]
    fn test_unicode_in_variable_value() {
        // Unicode characters should work fine in values
        let mut vars = HashMap::new();
        vars.insert("greeting".to_string(), "你好世界".to_string());
        vars.insert("emoji".to_string(), "🚀✨".to_string());

        assert_eq!(render_template("{{ greeting }}", &vars), "你好世界");
        assert_eq!(render_template("Hello {{ emoji }}", &vars), "Hello 🚀✨");
    }

    #[test]
    fn test_unicode_in_variable_name_not_matched() {
        // Unicode variable names should NOT match (per regex pattern)
        let mut vars = HashMap::new();
        vars.insert("变量".to_string(), "value".to_string());

        // The pattern requires ASCII letters/underscores, so this won't match
        assert_eq!(render_template("{{ 变量 }}", &vars), "{{ 变量 }}");
    }

    #[test]
    fn test_single_pass_substitution() {
        // Values containing {{ }} patterns should NOT be re-processed
        // This is important for security - prevents injection via values
        let mut vars = HashMap::new();
        vars.insert("safe".to_string(), "{{ injected }}".to_string());
        vars.insert("injected".to_string(), "SHOULD_NOT_APPEAR".to_string());

        // Only one substitution pass - the {{ injected }} in the value is literal
        assert_eq!(render_template("{{ safe }}", &vars), "{{ injected }}");
    }

    #[test]
    fn test_value_with_special_characters() {
        // Values can contain any characters, including shell metacharacters
        let mut vars = HashMap::new();
        vars.insert("cmd".to_string(), "$(echo test)".to_string());
        vars.insert("backticks".to_string(), "`whoami`".to_string());
        vars.insert("quotes".to_string(), "\"hello\" 'world'".to_string());

        // Values are preserved exactly as-is (no escaping)
        assert_eq!(render_template("{{ cmd }}", &vars), "$(echo test)");
        assert_eq!(render_template("{{ backticks }}", &vars), "`whoami`");
        assert_eq!(render_template("{{ quotes }}", &vars), "\"hello\" 'world'");
    }
}

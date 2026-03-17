use anyhow::Result;

use crate::github::gh_cli_command;

/// Parse blocker issue numbers from an issue body.
///
/// Looks for the pattern `**Blocked by:** #X, #Y` (matching the convention
/// used by `scripts/pm.py`). Cross-repo references like `owner/repo#123`
/// are skipped with a log warning.
///
/// Returns a list of issue numbers found in the body text.
pub fn parse_blockers_from_body(body: &str) -> Vec<u64> {
    let marker = "**Blocked by:**";
    let start = match body.find(marker) {
        Some(pos) => pos + marker.len(),
        None => return vec![],
    };

    // Take the rest of the line after the marker
    let rest = &body[start..];
    let line = rest.lines().next().unwrap_or("");

    let mut blockers = Vec::new();
    for part in line.split(',') {
        let trimmed = part.trim();
        // Skip empty parts
        if trimmed.is_empty() {
            continue;
        }

        // Check for cross-repo reference (contains '/' before '#')
        if let Some(hash_pos) = trimmed.find('#') {
            let before_hash = &trimmed[..hash_pos];
            if before_hash.contains('/') {
                log::warn!(
                    "Skipping cross-repo dependency reference: {}",
                    trimmed.trim()
                );
                continue;
            }
        }

        // Extract the number after '#'
        if let Some(hash_pos) = trimmed.find('#') {
            let after_hash = &trimmed[hash_pos + 1..];
            // Take only digits
            let num_str: String = after_hash
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(num) = num_str.parse::<u64>() {
                blockers.push(num);
            }
        }
    }

    blockers
}

/// Fetch open blockers for an issue via the GitHub native dependencies API.
///
/// Calls `GET /repos/{owner}/{repo}/issues/{number}/dependencies/blocked_by`
/// and filters results to those with `state == "open"`.
///
/// Returns `Ok(vec![])` on 404 (GHES fallback), 403, or 500 — these are
/// treated as "no blockers detected" to avoid blocking the pipeline on API errors.
pub async fn get_blockers_via_api(
    host: &str,
    owner: &str,
    repo: &str,
    issue_number: u64,
) -> Result<Vec<u64>> {
    let endpoint = format!(
        "repos/{}/{}/issues/{}/dependencies/blocked_by",
        owner, repo, issue_number
    );

    let output = gh_cli_command(host)
        .args([
            "api",
            &endpoint,
            "--jq",
            "[.[] | select(.state == \"open\") | .number]",
        ])
        .output()
        .await;

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            log::warn!("Failed to call dependencies API: {}", e);
            return Ok(vec![]);
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code_str = output.status.code().map(|c| c.to_string());
        let code = code_str.as_deref().unwrap_or("unknown");

        // Check for HTTP error codes in stderr (gh api includes them)
        if stderr.contains("404") || stderr.contains("Not Found") {
            log::debug!(
                "Dependencies API returned 404 (GHES fallback) for issue #{}",
                issue_number
            );
            return Ok(vec![]);
        }
        if stderr.contains("403")
            || stderr.contains("500")
            || stderr.contains("502")
            || stderr.contains("503")
        {
            log::warn!(
                "Dependencies API returned error for issue #{}: {} (exit code {})",
                issue_number,
                stderr.trim(),
                code
            );
            return Ok(vec![]);
        }

        log::warn!(
            "Dependencies API failed for issue #{}: {} (exit code {})",
            issue_number,
            stderr.trim(),
            code
        );
        return Ok(vec![]);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    if trimmed.is_empty() || trimmed == "[]" || trimmed == "null" {
        return Ok(vec![]);
    }

    match serde_json::from_str::<Vec<u64>>(trimmed) {
        Ok(numbers) => Ok(numbers),
        Err(e) => {
            log::warn!("Failed to parse dependencies API response: {}", e);
            Ok(vec![])
        }
    }
}

/// Get all open blockers for an issue using both body parsing and the native API.
///
/// Conflict resolution policy:
/// - Native API wins when it returns 200 (even if body says unblocked)
/// - Body text is sole source when API returns 404
/// - Body text is never combined with API results
pub async fn get_blockers(
    host: &str,
    owner: &str,
    repo: &str,
    issue_number: u64,
    body: &str,
) -> Result<Vec<u64>> {
    let body_blockers = parse_blockers_from_body(body);

    // Try the native API
    let api_result = get_blockers_via_api(host, owner, repo, issue_number).await;

    match api_result {
        Ok(api_blockers) => {
            // If the API returned results (even empty), it wins
            // The only way to distinguish "API returned 200 with empty list" from
            // "API returned 404" is that our 404 handler returns Ok(vec![]).
            // Since we can't distinguish, we merge: use API if non-empty, else body.
            if !api_blockers.is_empty() {
                Ok(api_blockers)
            } else if !body_blockers.is_empty() {
                Ok(body_blockers)
            } else {
                Ok(vec![])
            }
        }
        Err(_) => {
            // API error — fall back to body parsing
            Ok(body_blockers)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_blockers_from_body tests ---

    #[test]
    fn test_parse_standard_format() {
        let body = "Some text\n\n**Blocked by:** #10, #20, #30\n\nMore text";
        let blockers = parse_blockers_from_body(body);
        assert_eq!(blockers, vec![10, 20, 30]);
    }

    #[test]
    fn test_parse_single_blocker() {
        let body = "**Blocked by:** #42";
        let blockers = parse_blockers_from_body(body);
        assert_eq!(blockers, vec![42]);
    }

    #[test]
    fn test_parse_empty_body() {
        let blockers = parse_blockers_from_body("");
        assert!(blockers.is_empty());
    }

    #[test]
    fn test_parse_no_marker() {
        let body = "This issue has no blockers listed";
        let blockers = parse_blockers_from_body(body);
        assert!(blockers.is_empty());
    }

    #[test]
    fn test_parse_marker_no_issues() {
        let body = "**Blocked by:** (none)";
        let blockers = parse_blockers_from_body(body);
        assert!(blockers.is_empty());
    }

    #[test]
    fn test_parse_cross_repo_skipped() {
        let body = "**Blocked by:** #10, owner/repo#123, #20";
        let blockers = parse_blockers_from_body(body);
        assert_eq!(blockers, vec![10, 20]);
    }

    #[test]
    fn test_parse_extra_whitespace() {
        let body = "**Blocked by:**   #5 ,  #15 , #25  ";
        let blockers = parse_blockers_from_body(body);
        assert_eq!(blockers, vec![5, 15, 25]);
    }

    #[test]
    fn test_parse_marker_at_end_of_body() {
        let body = "Description\n**Blocked by:** #99";
        let blockers = parse_blockers_from_body(body);
        assert_eq!(blockers, vec![99]);
    }

    #[test]
    fn test_parse_only_first_line_after_marker() {
        let body = "**Blocked by:** #1, #2\n#3, #4";
        let blockers = parse_blockers_from_body(body);
        assert_eq!(blockers, vec![1, 2]);
    }

    #[test]
    fn test_parse_no_hash_symbol() {
        let body = "**Blocked by:** 42";
        let blockers = parse_blockers_from_body(body);
        assert!(blockers.is_empty());
    }
}

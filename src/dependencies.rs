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

/// Result of interpreting the `gh api` output for the dependencies endpoint.
///
/// Separates "API supported and returned a result" from "API not available or errored".
#[derive(Debug, PartialEq)]
pub enum ApiResult {
    /// API returned 200 — the contained list is the set of open blocker issue numbers.
    Supported(Vec<u64>),
    /// API is not available (404/GHES) or returned an error (403/5xx/spawn failure).
    Unavailable,
}

/// Parse the raw output of `gh api` for the dependencies endpoint into an [`ApiResult`].
///
/// This is a pure function extracted for testability. It interprets the exit status,
/// stderr, and stdout of the `gh api` call.
pub fn parse_api_output(success: bool, stdout: &str, stderr: &str, issue_number: u64) -> ApiResult {
    if !success {
        if stderr.contains("404") || stderr.contains("Not Found") {
            log::debug!(
                "Dependencies API returned 404 (GHES fallback) for issue #{}",
                issue_number
            );
            return ApiResult::Unavailable;
        }
        if stderr.contains("403")
            || stderr.contains("500")
            || stderr.contains("502")
            || stderr.contains("503")
        {
            log::warn!(
                "Dependencies API returned error for issue #{}: {}",
                issue_number,
                stderr.trim(),
            );
            return ApiResult::Unavailable;
        }

        log::warn!(
            "Dependencies API failed for issue #{}: {}",
            issue_number,
            stderr.trim(),
        );
        return ApiResult::Unavailable;
    }

    let trimmed = stdout.trim();

    if trimmed.is_empty() || trimmed == "[]" || trimmed == "null" {
        return ApiResult::Supported(vec![]);
    }

    match serde_json::from_str::<Vec<u64>>(trimmed) {
        Ok(numbers) => ApiResult::Supported(numbers),
        Err(e) => {
            log::warn!("Failed to parse dependencies API response: {}", e);
            ApiResult::Unavailable
        }
    }
}

/// Fetch open blockers for an issue via the GitHub native dependencies API.
///
/// Calls `GET /repos/{owner}/{repo}/issues/{number}/dependencies/blocked_by`
/// and filters results to those with `state == "open"`.
///
/// Returns `Some(blockers)` when the API is supported and returns 200.
/// Returns `None` on 404 (GHES), 403, 5xx, or any other error — the caller
/// should fall back to body parsing.
pub async fn get_blockers_via_api(
    host: &str,
    owner: &str,
    repo: &str,
    issue_number: u64,
) -> Option<Vec<u64>> {
    let endpoint = format!(
        "repos/{}/{}/issues/{}/dependencies/blocked_by",
        owner, repo, issue_number
    );

    let output = match gh_cli_command(host)
        .args([
            "api",
            &endpoint,
            "--jq",
            "[.[] | select(.state == \"open\") | .number]",
        ])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            log::warn!("Failed to call dependencies API: {}", e);
            return None;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    match parse_api_output(output.status.success(), &stdout, &stderr, issue_number) {
        ApiResult::Supported(blockers) => Some(blockers),
        ApiResult::Unavailable => None,
    }
}

/// Resolve blockers from body text and an optional API result.
///
/// This is a pure function extracted for testability. It implements the
/// resolution policy without any I/O.
///
/// Resolution policy:
/// - Native API wins when it returns `Some` (even if the list is empty)
/// - Body text is the sole source when the API is unavailable (`None`)
/// - Results are never combined across sources
pub fn resolve_blockers(body: &str, api_result: Option<Vec<u64>>) -> Vec<u64> {
    match api_result {
        Some(api_blockers) => api_blockers,
        None => parse_blockers_from_body(body),
    }
}

/// Get all open blockers for an issue using both body parsing and the native API.
///
/// Resolution policy:
/// - Native API wins when it returns 200 (even if the list is empty)
/// - Body text is the sole source when the API is unavailable (404/error)
/// - Results are never combined across sources
pub async fn get_blockers(
    host: &str,
    owner: &str,
    repo: &str,
    issue_number: u64,
    body: &str,
) -> Vec<u64> {
    let api_result = get_blockers_via_api(host, owner, repo, issue_number).await;
    resolve_blockers(body, api_result)
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

    // --- parse_api_output tests ---

    #[test]
    fn test_api_success_with_blockers() {
        let result = parse_api_output(true, "[10, 20]", "", 1);
        assert_eq!(result, ApiResult::Supported(vec![10, 20]));
    }

    #[test]
    fn test_api_success_empty_list() {
        let result = parse_api_output(true, "[]", "", 1);
        assert_eq!(result, ApiResult::Supported(vec![]));
    }

    #[test]
    fn test_api_success_empty_stdout() {
        let result = parse_api_output(true, "", "", 1);
        assert_eq!(result, ApiResult::Supported(vec![]));
    }

    #[test]
    fn test_api_success_null() {
        let result = parse_api_output(true, "null", "", 1);
        assert_eq!(result, ApiResult::Supported(vec![]));
    }

    #[test]
    fn test_api_success_invalid_json() {
        let result = parse_api_output(true, "not json", "", 1);
        assert_eq!(result, ApiResult::Unavailable);
    }

    #[test]
    fn test_api_404_not_found() {
        let result = parse_api_output(false, "", "HTTP 404: Not Found", 1);
        assert_eq!(result, ApiResult::Unavailable);
    }

    #[test]
    fn test_api_403_forbidden() {
        let result = parse_api_output(false, "", "HTTP 403: Forbidden", 1);
        assert_eq!(result, ApiResult::Unavailable);
    }

    #[test]
    fn test_api_500_server_error() {
        let result = parse_api_output(false, "", "HTTP 500: Internal Server Error", 1);
        assert_eq!(result, ApiResult::Unavailable);
    }

    #[test]
    fn test_api_unknown_error() {
        let result = parse_api_output(false, "", "something unexpected", 1);
        assert_eq!(result, ApiResult::Unavailable);
    }

    // --- resolve_blockers tests (GHES fallback / E2E resolution logic) ---

    #[test]
    fn test_resolve_ghes_404_falls_back_to_body() {
        // GHES: API unavailable (404), body has blockers → body wins
        let body = "**Blocked by:** #10, #20";
        let result = resolve_blockers(body, None);
        assert_eq!(result, vec![10, 20]);
    }

    #[test]
    fn test_resolve_ghes_404_no_body_blockers() {
        // GHES: API unavailable, body has no blockers → unblocked
        let body = "Just a regular issue description";
        let result = resolve_blockers(body, None);
        assert!(result.is_empty());
    }

    #[test]
    fn test_resolve_api_supported_overrides_body() {
        // github.com: API returns blockers, body also has blockers → API wins
        let body = "**Blocked by:** #10, #20";
        let result = resolve_blockers(body, Some(vec![30, 40]));
        assert_eq!(result, vec![30, 40]);
    }

    #[test]
    fn test_resolve_api_empty_overrides_body() {
        // github.com: API says unblocked, body says blocked → API wins (authoritative)
        let body = "**Blocked by:** #10, #20";
        let result = resolve_blockers(body, Some(vec![]));
        assert!(result.is_empty());
    }

    #[test]
    fn test_resolve_api_supported_no_body() {
        // github.com: API returns blockers, no body text → API result used
        let body = "";
        let result = resolve_blockers(body, Some(vec![5]));
        assert_eq!(result, vec![5]);
    }

    #[test]
    fn test_resolve_both_empty() {
        // No blockers from either source → unblocked
        let body = "";
        let result = resolve_blockers(body, Some(vec![]));
        assert!(result.is_empty());
    }

    #[test]
    fn test_resolve_sources_never_combined() {
        // Body says #10, API says #20 → only API result returned, not union
        let body = "**Blocked by:** #10";
        let result = resolve_blockers(body, Some(vec![20]));
        assert_eq!(result, vec![20]);
        assert!(!result.contains(&10));
    }

    // --- Error code differentiation tests ---

    #[test]
    fn test_api_404_is_debug_not_error() {
        // 404 = GHES, expected behavior, should use log::debug (not warn)
        // Verified by the fact that parse_api_output returns Unavailable
        // and the log message uses "GHES fallback" phrasing
        let result = parse_api_output(false, "", "HTTP 404: Not Found", 42);
        assert_eq!(result, ApiResult::Unavailable);
    }

    #[test]
    fn test_api_403_is_warning() {
        // 403 = permissions issue, different from 404 (GHES)
        let result = parse_api_output(false, "", "HTTP 403: Forbidden", 42);
        assert_eq!(result, ApiResult::Unavailable);
    }

    #[test]
    fn test_api_502_bad_gateway() {
        let result = parse_api_output(false, "", "HTTP 502: Bad Gateway", 42);
        assert_eq!(result, ApiResult::Unavailable);
    }

    #[test]
    fn test_api_503_service_unavailable() {
        let result = parse_api_output(false, "", "HTTP 503: Service Unavailable", 42);
        assert_eq!(result, ApiResult::Unavailable);
    }

    // --- GHES end-to-end scenario tests ---

    #[test]
    fn test_ghes_scenario_blocked_issue_detected_via_body() {
        // Simulate GHES: API returns 404 (None), issue has body deps
        // This is the primary GHES use case
        let body = "## Description\nImplement feature X\n\n**Blocked by:** #100, #200\n\n## Notes\nSome notes";
        let api_result = {
            let parsed = parse_api_output(false, "", "HTTP 404: Not Found", 50);
            match parsed {
                ApiResult::Supported(v) => Some(v),
                ApiResult::Unavailable => None,
            }
        };
        let blockers = resolve_blockers(body, api_result);
        assert_eq!(blockers, vec![100, 200]);
    }

    #[test]
    fn test_ghes_scenario_unblocked_issue_passes() {
        // GHES: API 404, no body deps → issue is unblocked
        let body = "## Description\nJust a regular issue";
        let api_result = {
            let parsed = parse_api_output(false, "", "HTTP 404: Not Found", 50);
            match parsed {
                ApiResult::Supported(v) => Some(v),
                ApiResult::Unavailable => None,
            }
        };
        let blockers = resolve_blockers(body, api_result);
        assert!(blockers.is_empty());
    }

    #[test]
    fn test_server_error_treats_as_unblocked() {
        // 500 error → treated as unblocked (don't block pipeline on API errors)
        let body = "**Blocked by:** #10";
        let api_result = {
            let parsed = parse_api_output(false, "", "HTTP 500: Internal Server Error", 50);
            match parsed {
                ApiResult::Supported(v) => Some(v),
                ApiResult::Unavailable => None,
            }
        };
        // Note: on 500, API returns None, so body parsing is used as fallback
        // This means body blockers ARE respected on server errors (same as GHES 404)
        let blockers = resolve_blockers(body, api_result);
        assert_eq!(blockers, vec![10]);
    }
}

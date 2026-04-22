//! GraphQL-based merge-readiness data fetching.
//!
//! Replaces 5+ REST calls with a single GraphQL query per merge-readiness
//! check. The `repository.pullRequest` query returns PR metadata, reviews,
//! check runs, legacy status contexts, and labels in one round-trip.
//!
//! GraphQL may be disabled or misconfigured on GHES installations; callers
//! are expected to fall back to REST when [`fetch_pr_merge_data`] returns
//! [`FetchOutcome::Unavailable`].

use crate::github;
use crate::github::DEFAULT_MAX_RETRIES;
use anyhow::{Context, Result};
use serde::Deserialize;

const QUERY: &str = r#"query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      isDraft
      mergeable
      headRefOid
      author { login }
      labels(first: 50) { nodes { name } }
      reviews(first: 100) {
        nodes {
          state
          author { login }
        }
        pageInfo { hasNextPage }
      }
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              contexts(first: 100) {
                nodes {
                  __typename
                  ... on CheckRun {
                    name
                    status
                    conclusion
                  }
                  ... on StatusContext {
                    context
                    state
                  }
                }
                pageInfo { hasNextPage }
              }
            }
          }
        }
      }
    }
  }
}"#;

/// Outcome of attempting to fetch merge-readiness data via GraphQL.
#[derive(Debug)]
pub(crate) enum FetchOutcome {
    /// GraphQL query succeeded and returned usable data.
    Available(MergeReadinessData),
    /// GraphQL endpoint is unavailable (e.g., disabled on GHES, 404/403).
    /// Callers should fall back to REST.
    Unavailable,
}

/// Merge-readiness data returned by the GraphQL query.
///
/// All enum-valued fields (states, conclusions, statuses) are lower-cased
/// here so downstream evaluation logic can share behavior with the REST path,
/// which returns already-lowercase strings.
#[derive(Debug)]
pub(crate) struct MergeReadinessData {
    // head_sha and labels are fetched by the query (per #727 acceptance
    // criteria) but not yet consumed by merge-readiness evaluation. Kept on
    // the struct so a follow-up can use the same query to avoid the separate
    // PR-details and labels REST calls elsewhere in pr_monitor.
    #[allow(dead_code)]
    pub head_sha: String,
    pub draft: bool,
    /// `Some(true)` when GitHub reports `MERGEABLE`, `Some(false)` for
    /// `CONFLICTING`, `None` for `UNKNOWN` (still computing).
    pub mergeable: Option<bool>,
    pub author_login: String,
    pub reviews: Vec<ReviewInfo>,
    pub check_runs: Vec<CheckRunInfo>,
    pub status_contexts: Vec<StatusContextInfo>,
    #[allow(dead_code)]
    pub labels: Vec<String>,
    /// `true` if any of the paginated sub-connections (reviews, rollup
    /// contexts) reported `hasNextPage`. The caller should treat this as a
    /// signal to fall back to REST so no data is silently dropped.
    pub has_more_pages: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ReviewInfo {
    pub state: String,
    pub author_login: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CheckRunInfo {
    pub status: Option<String>,
    pub conclusion: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct StatusContextInfo {
    pub state: String,
}

/// Fetch merge-readiness data for a PR via a single GraphQL query.
///
/// Returns [`FetchOutcome::Unavailable`] when the GraphQL endpoint cannot be
/// reached (404/403) or reports a transport-level error indicating GraphQL
/// is disabled. Query-level "not found" errors (e.g., PR doesn't exist) are
/// returned as `Err`.
pub(crate) async fn fetch_pr_merge_data(
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> Result<FetchOutcome> {
    let number_arg = format!("number={pr_number}");
    let owner_arg = format!("owner={owner}");
    let repo_arg = format!("repo={repo}");
    let query_arg = format!("query={QUERY}");

    let output = github::gh_api_with_retry(
        host,
        &[
            "api",
            "graphql",
            "-f",
            &query_arg,
            "-F",
            &owner_arg,
            "-F",
            &repo_arg,
            "-F",
            &number_arg,
        ],
        DEFAULT_MAX_RETRIES,
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_graphql_unavailable(&stderr) {
            log::info!(
                "GraphQL unavailable for {}/{} PR #{}: {}. Falling back to REST.",
                owner,
                repo,
                pr_number,
                stderr.trim()
            );
            return Ok(FetchOutcome::Unavailable);
        }
        anyhow::bail!(
            "GraphQL merge-readiness query failed for {}/{} PR #{}: {}",
            owner,
            repo,
            pr_number,
            stderr.trim()
        );
    }

    let parsed: GqlResponse = serde_json::from_slice(&output.stdout)
        .context("Failed to parse GraphQL merge-readiness response")?;

    parse_response(parsed).map(FetchOutcome::Available)
}

/// Returns `true` when `stderr` suggests the GraphQL endpoint itself is not
/// available (as opposed to a per-query error). This is how GHES installs
/// without GraphQL typically present themselves.
fn is_graphql_unavailable(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    // 404/403 on the /graphql endpoint itself, or "not supported" messaging.
    (lower.contains("404") && lower.contains("not found"))
        || (lower.contains("403") && lower.contains("forbidden"))
        || lower.contains("graphql is not supported")
        || lower.contains("graphql not enabled")
}

fn parse_response(resp: GqlResponse) -> Result<MergeReadinessData> {
    if !resp.errors.is_empty() {
        let messages: Vec<String> = resp.errors.iter().map(|e| e.message.clone()).collect();
        anyhow::bail!("GraphQL query returned errors: {}", messages.join("; "));
    }

    let repository = resp
        .data
        .and_then(|d| d.repository)
        .context("GraphQL response missing repository")?;
    let pr = repository
        .pull_request
        .context("GraphQL response missing pullRequest")?;

    let mergeable = match pr.mergeable.as_str() {
        "MERGEABLE" => Some(true),
        "CONFLICTING" => Some(false),
        _ => None, // UNKNOWN or anything unexpected
    };

    let author_login = pr.author.map(|a| a.login).unwrap_or_default();

    let mut has_more_pages = pr.reviews.page_info.has_next_page;

    let reviews: Vec<ReviewInfo> = pr
        .reviews
        .nodes
        .into_iter()
        .map(|r| ReviewInfo {
            state: r.state,
            author_login: r.author.map(|a| a.login).unwrap_or_default(),
        })
        .collect();

    let labels: Vec<String> = pr.labels.nodes.into_iter().map(|l| l.name).collect();

    let mut check_runs: Vec<CheckRunInfo> = Vec::new();
    let mut status_contexts: Vec<StatusContextInfo> = Vec::new();
    if let Some(commit_node) = pr.commits.nodes.into_iter().next() {
        if let Some(rollup) = commit_node.commit.status_check_rollup {
            if rollup.contexts.page_info.has_next_page {
                has_more_pages = true;
            }
            for ctx in rollup.contexts.nodes {
                match ctx.typename.as_str() {
                    "CheckRun" => check_runs.push(CheckRunInfo {
                        status: ctx.status.map(|s| normalize_enum(&s)),
                        conclusion: ctx.conclusion.map(|c| normalize_enum(&c)),
                    }),
                    "StatusContext" => {
                        if let Some(state) = ctx.state {
                            status_contexts.push(StatusContextInfo {
                                state: normalize_enum(&state),
                            });
                        }
                    }
                    // Unknown context type — ignore. Unknown types contribute
                    // no signal, and silently including them would risk
                    // misclassifying CI state.
                    _ => {}
                }
            }
        }
    }

    Ok(MergeReadinessData {
        head_sha: pr.head_ref_oid,
        draft: pr.is_draft,
        mergeable,
        author_login,
        reviews,
        check_runs,
        status_contexts,
        labels,
        has_more_pages,
    })
}

/// GraphQL enum values are SCREAMING_SNAKE_CASE. Lower-case them to match the
/// REST representation used by the existing evaluation logic.
fn normalize_enum(s: &str) -> String {
    s.to_ascii_lowercase()
}

// --- GraphQL response types ---

#[derive(Debug, Deserialize)]
struct GqlResponse {
    #[serde(default)]
    data: Option<GqlData>,
    #[serde(default)]
    errors: Vec<GqlError>,
}

#[derive(Debug, Deserialize)]
struct GqlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct GqlData {
    repository: Option<GqlRepository>,
}

#[derive(Debug, Deserialize)]
struct GqlRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<GqlPullRequest>,
}

#[derive(Debug, Deserialize)]
struct GqlPullRequest {
    #[serde(rename = "isDraft")]
    is_draft: bool,
    mergeable: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    author: Option<GqlAuthor>,
    labels: GqlLabelConnection,
    reviews: GqlReviewConnection,
    commits: GqlCommitConnection,
}

#[derive(Debug, Deserialize)]
struct GqlAuthor {
    #[serde(default)]
    login: String,
}

#[derive(Debug, Deserialize)]
struct GqlLabelConnection {
    #[serde(default)]
    nodes: Vec<GqlLabel>,
}

#[derive(Debug, Deserialize)]
struct GqlLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GqlReviewConnection {
    #[serde(default)]
    nodes: Vec<GqlReview>,
    #[serde(rename = "pageInfo", default)]
    page_info: GqlPageInfo,
}

#[derive(Debug, Default, Deserialize)]
struct GqlPageInfo {
    #[serde(rename = "hasNextPage", default)]
    has_next_page: bool,
}

#[derive(Debug, Deserialize)]
struct GqlReview {
    state: String,
    author: Option<GqlAuthor>,
}

#[derive(Debug, Deserialize)]
struct GqlCommitConnection {
    #[serde(default)]
    nodes: Vec<GqlCommitNode>,
}

#[derive(Debug, Deserialize)]
struct GqlCommitNode {
    commit: GqlCommit,
}

#[derive(Debug, Deserialize)]
struct GqlCommit {
    #[serde(rename = "statusCheckRollup", default)]
    status_check_rollup: Option<GqlRollup>,
}

#[derive(Debug, Deserialize)]
struct GqlRollup {
    contexts: GqlContextConnection,
}

#[derive(Debug, Deserialize)]
struct GqlContextConnection {
    #[serde(default)]
    nodes: Vec<GqlContext>,
    #[serde(rename = "pageInfo", default)]
    page_info: GqlPageInfo,
}

/// Flattened context node. Accepts both `CheckRun` and `StatusContext`
/// variants from the `StatusCheckRollupContext` union.
#[derive(Debug, Deserialize)]
struct GqlContext {
    #[serde(rename = "__typename")]
    typename: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<MergeReadinessData> {
        let resp: GqlResponse = serde_json::from_str(json).expect("valid json");
        parse_response(resp)
    }

    #[test]
    fn parses_typical_successful_response() {
        let json = r#"{
          "data": {
            "repository": {
              "pullRequest": {
                "isDraft": false,
                "mergeable": "MERGEABLE",
                "headRefOid": "deadbeef",
                "author": { "login": "alice" },
                "labels": { "nodes": [{"name":"enhancement"},{"name":"gru:in-progress"}] },
                "reviews": {
                  "nodes": [
                    {"state":"APPROVED","author":{"login":"bob"}}
                  ],
                  "pageInfo": {"hasNextPage": false}
                },
                "commits": {
                  "nodes": [{
                    "commit": {
                      "statusCheckRollup": {
                        "contexts": {
                          "nodes": [
                            {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS"},
                            {"__typename":"StatusContext","context":"ci/legacy","state":"SUCCESS"}
                          ],
                          "pageInfo": {"hasNextPage": false}
                        }
                      }
                    }
                  }]
                }
              }
            }
          }
        }"#;

        let data = parse(json).unwrap();
        assert_eq!(data.head_sha, "deadbeef");
        assert!(!data.draft);
        assert_eq!(data.mergeable, Some(true));
        assert_eq!(data.author_login, "alice");
        assert_eq!(data.labels, vec!["enhancement", "gru:in-progress"]);
        assert_eq!(data.reviews.len(), 1);
        assert_eq!(data.reviews[0].state, "APPROVED");
        assert_eq!(data.reviews[0].author_login, "bob");
        assert_eq!(data.check_runs.len(), 1);
        assert_eq!(data.check_runs[0].status.as_deref(), Some("completed"));
        assert_eq!(data.check_runs[0].conclusion.as_deref(), Some("success"));
        assert_eq!(data.status_contexts.len(), 1);
        assert_eq!(data.status_contexts[0].state, "success");
        assert!(!data.has_more_pages);
    }

    #[test]
    fn parses_conflicting_and_unknown_mergeable() {
        let conflicting = r#"{"data":{"repository":{"pullRequest":{
            "isDraft": false,
            "mergeable": "CONFLICTING",
            "headRefOid": "abc",
            "author": {"login":"a"},
            "labels": {"nodes": []},
            "reviews": {"nodes": [], "pageInfo":{"hasNextPage":false}},
            "commits": {"nodes": []}
        }}}}"#;
        assert_eq!(parse(conflicting).unwrap().mergeable, Some(false));

        let unknown = r#"{"data":{"repository":{"pullRequest":{
            "isDraft": false,
            "mergeable": "UNKNOWN",
            "headRefOid": "abc",
            "author": {"login":"a"},
            "labels": {"nodes": []},
            "reviews": {"nodes": [], "pageInfo":{"hasNextPage":false}},
            "commits": {"nodes": []}
        }}}}"#;
        assert_eq!(parse(unknown).unwrap().mergeable, None);
    }

    #[test]
    fn parses_draft_pr() {
        let json = r#"{"data":{"repository":{"pullRequest":{
            "isDraft": true,
            "mergeable": "UNKNOWN",
            "headRefOid": "abc",
            "author": {"login":"a"},
            "labels": {"nodes": []},
            "reviews": {"nodes": [], "pageInfo":{"hasNextPage":false}},
            "commits": {"nodes": []}
        }}}}"#;
        let data = parse(json).unwrap();
        assert!(data.draft);
    }

    #[test]
    fn parses_null_rollup() {
        // A brand-new PR with no commits yet or no CI wired up — statusCheckRollup is null.
        let json = r#"{"data":{"repository":{"pullRequest":{
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "headRefOid": "abc",
            "author": {"login":"a"},
            "labels": {"nodes": []},
            "reviews": {"nodes": [], "pageInfo":{"hasNextPage":false}},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": null}}]}
        }}}}"#;
        let data = parse(json).unwrap();
        assert!(data.check_runs.is_empty());
        assert!(data.status_contexts.is_empty());
    }

    #[test]
    fn parses_null_author_login() {
        // Ghost/deleted users yield null author. Should not crash.
        let json = r#"{"data":{"repository":{"pullRequest":{
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "headRefOid": "abc",
            "author": null,
            "labels": {"nodes": []},
            "reviews": {"nodes": [
              {"state":"APPROVED","author": null}
            ], "pageInfo":{"hasNextPage":false}},
            "commits": {"nodes": []}
        }}}}"#;
        let data = parse(json).unwrap();
        assert_eq!(data.author_login, "");
        assert_eq!(data.reviews[0].author_login, "");
    }

    #[test]
    fn flags_pagination_overflow() {
        let json = r#"{"data":{"repository":{"pullRequest":{
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "headRefOid": "abc",
            "author": {"login":"a"},
            "labels": {"nodes": []},
            "reviews": {"nodes": [], "pageInfo":{"hasNextPage":true}},
            "commits": {"nodes": []}
        }}}}"#;
        assert!(parse(json).unwrap().has_more_pages);
    }

    #[test]
    fn flags_pagination_overflow_on_rollup() {
        let json = r#"{"data":{"repository":{"pullRequest":{
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "headRefOid": "abc",
            "author": {"login":"a"},
            "labels": {"nodes": []},
            "reviews": {"nodes": [], "pageInfo":{"hasNextPage":false}},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {
              "contexts": {
                "nodes": [],
                "pageInfo": {"hasNextPage": true}
              }
            }}}]}
        }}}}"#;
        assert!(parse(json).unwrap().has_more_pages);
    }

    #[test]
    fn ignores_unknown_context_types() {
        let json = r#"{"data":{"repository":{"pullRequest":{
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "headRefOid": "abc",
            "author": {"login":"a"},
            "labels": {"nodes": []},
            "reviews": {"nodes": [], "pageInfo":{"hasNextPage":false}},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {
              "contexts": {
                "nodes": [
                  {"__typename":"SomeFutureType"},
                  {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"}
                ],
                "pageInfo": {"hasNextPage": false}
              }
            }}}]}
        }}}}"#;
        let data = parse(json).unwrap();
        assert_eq!(data.check_runs.len(), 1);
    }

    #[test]
    fn surfaces_graphql_errors() {
        let json = r#"{
          "errors": [{"message": "Could not resolve to a PullRequest"}],
          "data": null
        }"#;
        let err = parse(json).unwrap_err();
        assert!(err.to_string().contains("Could not resolve"));
    }

    #[test]
    fn unavailable_detection_matches_ghes_responses() {
        assert!(is_graphql_unavailable(
            "HTTP 404: Not Found (https://ghes.example.com/api/graphql)"
        ));
        assert!(is_graphql_unavailable(
            "HTTP 403: Forbidden (graphql disabled)"
        ));
        assert!(is_graphql_unavailable(
            "GraphQL is not supported on this server"
        ));
        assert!(!is_graphql_unavailable("500 Internal Server Error"));
        assert!(!is_graphql_unavailable("rate limit exceeded"));
    }
}

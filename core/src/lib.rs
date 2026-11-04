//! Core of GitHub Viewer: talks to the GitHub GraphQL API and exposes plain
//! data types to the Swift app through UniFFI.

use std::path::Path;
use std::time::SystemTime;

use serde::Deserialize;

uniffi::setup_scaffolding!();

const GRAPHQL_URL: &str = "https://api.github.com/graphql";

/// Both dashboard lists in a single round trip. `archived:false` hides PRs in
/// archived repos, which can never be merged or reviewed anyway.
const DASHBOARD_QUERY: &str = r#"
query {
  viewer { login }
  authored: search(query: "is:pr is:open archived:false author:@me sort:updated-desc", type: ISSUE, first: 50) {
    nodes { ...pr }
  }
  reviewRequested: search(query: "is:pr is:open archived:false review-requested:@me sort:updated-desc", type: ISSUE, first: 50) {
    nodes { ...pr }
  }
}

fragment pr on PullRequest {
  number
  title
  url
  isDraft
  updatedAt
  reviewDecision
  repository { nameWithOwner }
  author { login }
  commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
}
"#;

/// Apps launched from Finder don't inherit the shell's PATH, so look in the
/// usual Homebrew locations before falling back to a PATH lookup.
const GH_CANDIDATES: &[&str] = &["/opt/homebrew/bin/gh", "/usr/local/bin/gh"];

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum CoreError {
    #[error("GitHub CLI (gh) not found. Install it with `brew install gh`.")]
    GhNotFound,
    #[error("Not signed in: {0}. Run `gh auth login` in a terminal.")]
    Auth(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("GitHub API error: {0}")]
    Api(String),
}

/// The overall review state GitHub computes from reviews and branch protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// Combined CI state of the PR's head commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CheckState {
    Success,
    Failure,
    Pending,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// "owner/name"
    pub repo: String,
    /// `None` when the author's account was deleted.
    pub author: Option<String>,
    pub is_draft: bool,
    /// `None` when the repo doesn't require reviews and nobody has reviewed yet.
    pub review_decision: Option<ReviewDecision>,
    /// `None` when the head commit has no checks.
    pub check_state: Option<CheckState>,
    pub updated_at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct Dashboard {
    pub viewer_login: String,
    pub authored: Vec<PullRequest>,
    pub review_requested: Vec<PullRequest>,
}

#[derive(uniffi::Object)]
pub struct GitHubClient {
    http: reqwest::Client,
}

#[uniffi::export(async_runtime = "tokio")]
impl GitHubClient {
    #[uniffi::constructor]
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("github-viewer/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("default reqwest client should build");
        Self { http }
    }

    /// Fetches the viewer's open PRs and the PRs awaiting their review.
    pub async fn fetch_dashboard(&self) -> Result<Dashboard, CoreError> {
        // Read the token on every call so `gh auth login`/`switch` takes effect without a restart.
        let token = gh_token().await?;
        let response = self
            .http
            .post(GRAPHQL_URL)
            .bearer_auth(token)
            .json(&serde_json::json!({ "query": DASHBOARD_QUERY }))
            .send()
            .await
            .map_err(|e| CoreError::Network(e.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| CoreError::Network(e.to_string()))?;

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(CoreError::Auth("GitHub rejected the gh token".into()));
        }
        if !status.is_success() {
            return Err(CoreError::Api(format!("HTTP {status}")));
        }
        parse_dashboard(&body)
    }
}

impl Default for GitHubClient {
    fn default() -> Self {
        Self::new()
    }
}

async fn gh_token() -> Result<String, CoreError> {
    let gh = GH_CANDIDATES
        .iter()
        .copied()
        .find(|path| Path::new(path).exists())
        .unwrap_or("gh");

    let output = tokio::process::Command::new(gh)
        .args(["auth", "token"])
        .output()
        .await
        .map_err(|_| CoreError::GhNotFound)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(CoreError::Auth(stderr));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(CoreError::Auth("gh returned an empty token".into()));
    }
    Ok(token)
}

// --- GraphQL response shapes -------------------------------------------------

#[derive(Deserialize)]
struct GraphQlResponse {
    data: Option<Data>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Data {
    viewer: Viewer,
    authored: Search,
    review_requested: Search,
}

#[derive(Deserialize)]
struct Viewer {
    login: String,
}

#[derive(Deserialize)]
struct Search {
    // Nodes come back as null when an org blocks the token (e.g. SAML SSO).
    nodes: Vec<Option<PrNode>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    number: u64,
    title: String,
    url: String,
    is_draft: bool,
    updated_at: String,
    review_decision: Option<String>,
    repository: Repository,
    author: Option<Author>,
    commits: Commits,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

#[derive(Deserialize)]
struct Commits {
    nodes: Vec<CommitNode>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: Commit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    status_check_rollup: Option<StatusCheckRollup>,
}

#[derive(Deserialize)]
struct StatusCheckRollup {
    state: String,
}

fn parse_dashboard(body: &str) -> Result<Dashboard, CoreError> {
    let response: GraphQlResponse = serde_json::from_str(body)
        .map_err(|e| CoreError::Api(format!("unexpected response: {e}")))?;

    // GraphQL can return errors alongside usable data (e.g. one org blocks the
    // token via SAML). Show what we got; only fail when there's no data at all.
    let Some(data) = response.data else {
        let message = response
            .errors
            .and_then(|errors| errors.into_iter().next())
            .map(|error| error.message)
            .unwrap_or_else(|| "response contained no data".into());
        return Err(CoreError::Api(message));
    };

    Ok(Dashboard {
        viewer_login: data.viewer.login,
        authored: convert_search(data.authored)?,
        review_requested: convert_search(data.review_requested)?,
    })
}

fn convert_search(search: Search) -> Result<Vec<PullRequest>, CoreError> {
    let mut pull_requests = search
        .nodes
        .into_iter()
        .flatten()
        .map(convert_pr)
        .collect::<Result<Vec<_>, _>>()?;
    // The query's `sort:updated-desc` uses GitHub's search index, which lags
    // behind `updatedAt`, so sort again on the real timestamps.
    pull_requests.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(pull_requests)
}

fn convert_pr(node: PrNode) -> Result<PullRequest, CoreError> {
    let updated_at = humantime::parse_rfc3339(&node.updated_at)
        .map_err(|e| CoreError::Api(format!("bad updatedAt {:?}: {e}", node.updated_at)))?;

    let review_decision = match node.review_decision.as_deref() {
        Some("APPROVED") => Some(ReviewDecision::Approved),
        Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
        Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
        _ => None,
    };

    let check_state = node
        .commits
        .nodes
        .into_iter()
        .next()
        .and_then(|node| node.commit.status_check_rollup)
        .and_then(|rollup| match rollup.state.as_str() {
            "SUCCESS" => Some(CheckState::Success),
            "FAILURE" | "ERROR" => Some(CheckState::Failure),
            "PENDING" | "EXPECTED" => Some(CheckState::Pending),
            _ => None,
        });

    Ok(PullRequest {
        number: node.number,
        title: node.title,
        url: node.url,
        repo: node.repository.name_with_owner,
        author: node.author.map(|author| author.login),
        is_draft: node.is_draft,
        review_decision,
        check_state,
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    const FULL_RESPONSE: &str = r#"{
      "data": {
        "viewer": { "login": "octocat" },
        "authored": { "nodes": [
          {
            "number": 12, "title": "Add parser", "url": "https://github.com/o/r/pull/12",
            "isDraft": false, "updatedAt": "2026-11-04T20:57:47Z", "reviewDecision": "APPROVED",
            "repository": { "nameWithOwner": "o/r" }, "author": { "login": "octocat" },
            "commits": { "nodes": [ { "commit": { "statusCheckRollup": { "state": "SUCCESS" } } } ] }
          },
          {
            "number": 13, "title": "WIP", "url": "https://github.com/o/r/pull/13",
            "isDraft": true, "updatedAt": "2026-11-03T04:01:13Z", "reviewDecision": null,
            "repository": { "nameWithOwner": "o/r" }, "author": { "login": "octocat" },
            "commits": { "nodes": [ { "commit": { "statusCheckRollup": null } } ] }
          }
        ] },
        "reviewRequested": { "nodes": [
          {
            "number": 7, "title": "Fix bug", "url": "https://github.com/x/y/pull/7",
            "isDraft": false, "updatedAt": "2026-11-01T00:00:00Z", "reviewDecision": "CHANGES_REQUESTED",
            "repository": { "nameWithOwner": "x/y" }, "author": null,
            "commits": { "nodes": [ { "commit": { "statusCheckRollup": { "state": "ERROR" } } } ] }
          }
        ] }
      }
    }"#;

    #[test]
    fn parses_both_lists() {
        let dashboard = parse_dashboard(FULL_RESPONSE).unwrap();
        assert_eq!(dashboard.viewer_login, "octocat");
        assert_eq!(dashboard.authored.len(), 2);
        assert_eq!(dashboard.review_requested.len(), 1);

        let first = &dashboard.authored[0];
        assert_eq!(first.number, 12);
        assert_eq!(first.title, "Add parser");
        assert_eq!(first.url, "https://github.com/o/r/pull/12");
        assert_eq!(first.repo, "o/r");
        assert_eq!(first.author.as_deref(), Some("octocat"));
        assert!(!first.is_draft);
        assert_eq!(first.updated_at, UNIX_EPOCH + Duration::from_secs(1_793_825_867));
    }

    #[test]
    fn maps_review_and_check_states() {
        let dashboard = parse_dashboard(FULL_RESPONSE).unwrap();

        let approved = &dashboard.authored[0];
        assert_eq!(approved.review_decision, Some(ReviewDecision::Approved));
        assert_eq!(approved.check_state, Some(CheckState::Success));

        let draft = &dashboard.authored[1];
        assert!(draft.is_draft);
        assert_eq!(draft.review_decision, None);
        assert_eq!(draft.check_state, None);

        let changes = &dashboard.review_requested[0];
        assert_eq!(changes.review_decision, Some(ReviewDecision::ChangesRequested));
        assert_eq!(changes.check_state, Some(CheckState::Failure));
        assert_eq!(changes.author, None);
    }

    #[test]
    fn keeps_partial_data_when_an_org_blocks_the_token() {
        let body = r#"{
          "data": {
            "viewer": { "login": "octocat" },
            "authored": { "nodes": [ null ] },
            "reviewRequested": { "nodes": [] }
          },
          "errors": [ { "message": "Resource protected by organization SAML enforcement." } ]
        }"#;
        let dashboard = parse_dashboard(body).unwrap();
        assert!(dashboard.authored.is_empty());
        assert!(dashboard.review_requested.is_empty());
    }

    #[test]
    fn reports_error_when_there_is_no_data() {
        let body = r#"{ "data": null, "errors": [ { "message": "Bad credentials" } ] }"#;
        let error = parse_dashboard(body).unwrap_err();
        assert_eq!(error.to_string(), "GitHub API error: Bad credentials");
    }

    #[test]
    fn sorts_by_updated_at_newest_first() {
        // GitHub's search index lags behind `updatedAt`, so `sort:updated-desc` isn't reliable.
        let node = |number: u64, updated_at: &str| {
            format!(
                r#"{{ "number": {number}, "title": "t", "url": "u{number}", "isDraft": false,
                     "updatedAt": "{updated_at}", "reviewDecision": null,
                     "repository": {{ "nameWithOwner": "o/r" }}, "author": null,
                     "commits": {{ "nodes": [] }} }}"#
            )
        };
        let body = format!(
            r#"{{ "data": {{ "viewer": {{ "login": "octocat" }},
                 "authored": {{ "nodes": [ {}, {}, {} ] }},
                 "reviewRequested": {{ "nodes": [] }} }} }}"#,
            node(1, "2026-10-20T18:45:45Z"),
            node(2, "2026-10-26T14:39:50Z"),
            node(3, "2026-10-21T15:53:32Z"),
        );
        let numbers: Vec<u64> = parse_dashboard(&body)
            .unwrap()
            .authored
            .iter()
            .map(|pr| pr.number)
            .collect();
        assert_eq!(numbers, [2, 3, 1]);
    }

    /// Hits the real API with the local `gh` login: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs network and a `gh auth login` session"]
    async fn fetches_live_dashboard() {
        let dashboard = GitHubClient::new().fetch_dashboard().await.unwrap();
        assert!(!dashboard.viewer_login.is_empty());
    }
}

//! PR details, review threads, viewed state, local drafts and review submission.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Author, CoreError, DiffSide, GitHubClient, GraphQlError};

/// The PR and the first page of every connection. Later pages reuse it with
/// `@include` turned off for connections that are done, so each round trip
/// only fetches what's left. Sent with `PENDING_REVIEWS` appended.
const PR_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!,
      $withFiles: Boolean!, $filesAfter: String, $withThreads: Boolean!, $threadsAfter: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id title body url isDraft viewerDidAuthor baseRefName headRefName headRefOid
      author { login }
      ...pendingReviews
      # Submitted reviews, oldest first; yours are picked out by viewerDidAuthor.
      lastReviews: reviews(last: 30, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) {
        nodes { viewerDidAuthor commit { oid } }
      }
      commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 50) { nodes {
        ... on CheckRun { name conclusion detailsUrl }
        ... on StatusContext { context state targetUrl }
      } } } } } }
      # GitHub caps this connection at 3000 files.
      files(first: 100, after: $filesAfter) @include(if: $withFiles) {
        pageInfo { hasNextPage endCursor }
        nodes { path viewerViewedState }
      }
      reviewThreads(first: 100, after: $threadsAfter) @include(if: $withThreads) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id path line startLine diffSide startDiffSide isResolved isOutdated
          comments(first: 100) { nodes { author { login } body createdAt url state commit { oid } } }
        }
      }
    }
  }
}
"#;

/// GitHub only shows a pending review to its author, and allows one per user;
/// `viewerDidAuthor` is checked anyway. Review comments carry no side, so
/// `diffHunk` is fetched to infer it.
const PENDING_REVIEWS: &str = r#"
fragment pendingReviews on PullRequest {
  reviews(states: PENDING, first: 10) {
    nodes {
      id viewerDidAuthor commit { oid }
      comments(first: 100) {
        nodes { path line startLine originalLine originalStartLine body diffHunk }
      }
    }
  }
}
"#;

/// The viewer's pending review, looked up right before submitting. Sent with
/// `PENDING_REVIEWS` appended.
const PENDING_LOOKUP: &str = r#"
query($id: ID!) { node(id: $id) { ... on PullRequest { ...pendingReviews } } }
"#;

const ADD_THREAD: &str = r#"
mutation($input: AddPullRequestReviewThreadInput!) {
  addPullRequestReviewThread(input: $input) { thread { id } }
}
"#;

const SUBMIT_PENDING: &str = r#"
mutation($input: SubmitPullRequestReviewInput!) {
  submitPullRequestReview(input: $input) { pullRequestReview { url } }
}
"#;

const MARK_VIEWED: &str = r#"
mutation($input: MarkFileAsViewedInput!) { markFileAsViewed(input: $input) { clientMutationId } }
"#;

const UNMARK_VIEWED: &str = r#"
mutation($input: UnmarkFileAsViewedInput!) { unmarkFileAsViewed(input: $input) { clientMutationId } }
"#;

/// Without an event, this starts a pending review. Submitting is a second step,
/// so a retry after a lost answer finds the review instead of posting another.
const CREATE_PENDING: &str = r#"
mutation($input: AddPullRequestReviewInput!) {
  addPullRequestReview(input: $input) { pullRequestReview { id url } }
}
"#;

/// The account gh is signed in as. A submit records it.
const VIEWER: &str = "query { viewer { id login } }";

/// The PR's head, read before drafts are added to a pending review.
const PR_HEAD: &str = "query($id: ID!) { node(id: $id) { ... on PullRequest { headRefOid } } }";

/// A review by id with a page of its comments: an earlier submit's, or the one a
/// submit adds drafts to.
const REMOTE_REVIEW: &str = r#"
query($id: ID!, $after: String) {
  node(id: $id) {
    ... on PullRequestReview {
      state url body viewerDidAuthor commit { oid }
      comments(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { path line startLine originalLine originalStartLine body diffHunk }
      }
    }
  }
}
"#;

/// Everything the review window shows about one PR, apart from the diff.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PullRequestDetail {
    /// GraphQL node id, used by the mutations.
    pub id: String,
    /// "owner/name"
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    /// `None` when the author's account was deleted.
    pub author: Option<String>,
    /// You opened this PR, so GitHub refuses your approvals and change requests on it.
    pub viewer_did_author: bool,
    pub is_draft: bool,
    pub base_ref_name: String,
    pub head_ref_name: String,
    /// GitHub's head commit when this was fetched. Thread lines usually refer to
    /// it; each thread's `commit_oid` says.
    pub head_oid: String,
    pub files: Vec<ChangedFile>,
    pub threads: Vec<ReviewThread>,
    /// A review you started on github.com and haven't submitted. Submitting
    /// adds your drafts to it and submits it.
    pub pending_review: Option<PendingReview>,
    /// The head commit's checks that failed, for saying which instead of only "checks failing".
    pub failed_checks: Vec<FailedCheck>,
    /// The commit of the last review you submitted, if any.
    pub last_reviewed_oid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FailedCheck {
    pub name: String,
    /// Its page on GitHub or at the CI provider, if it has one.
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PendingReview {
    pub id: String,
    /// The commit the review was started on. Empty if GitHub no longer has it.
    pub commit_oid: String,
    pub comments: Vec<PendingComment>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PendingComment {
    pub path: String,
    /// `None` for a comment on the whole file.
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    /// The range on the commit its review was started on. Unlike `line`, it
    /// doesn't move with later pushes, so drafts are matched on it.
    pub original_line: Option<u32>,
    pub original_start_line: Option<u32>,
    /// Inferred from the diff hunk, as GitHub doesn't report it for review
    /// comments: `Left` on a removed line, `Right` on an added one, and `None`
    /// on a context line, which a comment may be on either side of.
    pub side: Option<DiffSide>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    #[serde(rename = "viewerViewedState")]
    pub viewed: ViewedState,
}

/// GitHub's per-file "Viewed" checkbox, for the signed-in user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ViewedState {
    Viewed,
    Unviewed,
    /// Was viewed, but the file changed since.
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ReviewThread {
    pub id: String,
    pub path: String,
    /// `None` when the thread is outdated and its line is no longer in the diff.
    pub line: Option<u32>,
    /// First line of the thread's range. GitHub sets it to `line` for a single-line thread.
    pub start_line: Option<u32>,
    pub side: DiffSide,
    pub start_side: Option<DiffSide>,
    pub is_resolved: bool,
    pub is_outdated: bool,
    /// The commit `line` and `start_line` refer to. Usually the PR's head, but a
    /// thread posted on an older commit stays on it until GitHub moves it, at the
    /// next push. `None` if GitHub doesn't say.
    pub commit_oid: Option<String>,
    pub comments: Vec<ThreadComment>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ThreadComment {
    /// `None` when the author's account was deleted.
    pub author: Option<String>,
    pub body: String,
    pub created_at: SystemTime,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum, Serialize, Deserialize)]
pub enum ReviewEvent {
    Approve,
    RequestChanges,
    Comment,
}

/// A review comment that hasn't been sent to GitHub yet.
#[derive(Debug, Clone, PartialEq, uniffi::Record, Serialize, Deserialize)]
pub struct DraftComment {
    pub id: String,
    pub path: String,
    /// Last line of the comment's range, as GitHub numbers lines on `side`.
    pub line: u32,
    pub side: DiffSide,
    /// First line, for a multi-line comment.
    pub start_line: Option<u32>,
    pub start_side: Option<DiffSide>,
    pub body: String,
    pub author: DraftAuthor,
    /// The commit whose diff the lines refer to. All of a PR's drafts share one.
    pub commit_oid: String,
}

/// A submit whose drafts are in your pending review on GitHub, not yet settled.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct UnsettledSubmit {
    pub review_url: String,
    /// The drafts in that review. They can't be edited or deleted here.
    pub draft_ids: Vec<String>,
}

/// Input to `DraftStore::add`: a draft before it has an id.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct NewDraft {
    pub path: String,
    pub line: u32,
    pub side: DiffSide,
    pub start_line: Option<u32>,
    pub start_side: Option<DiffSide>,
    pub body: String,
    pub author: DraftAuthor,
    pub commit_oid: String,
}

/// Who wrote a draft: you, or an agent through the MCP server.
#[derive(Debug, Clone, PartialEq, uniffi::Enum, Serialize, Deserialize)]
pub enum DraftAuthor {
    You,
    Agent { name: String },
}

#[uniffi::export(async_runtime = "tokio")]
impl GitHubClient {
    /// Everything the review window needs about one PR. `repo` is "owner/name".
    pub async fn fetch_pull_request(
        &self,
        repo: String,
        number: u64,
    ) -> Result<PullRequestDetail, CoreError> {
        let (owner, name) = split_repo(&repo).ok_or_else(|| {
            CoreError::Api(format!("invalid repo {repo:?}, expected \"owner/name\""))
        })?;
        let query = format!("{PR_QUERY}{PENDING_REVIEWS}");
        // A push between pages mixes two heads' threads, so that read starts over, once.
        for _ in 0..2 {
            let mut variables = json!({
                "owner": owner, "name": name, "number": number,
                "withFiles": true, "withThreads": true,
            });
            let first = parse_pr_page(
                &self.graphql(&query, variables.clone()).await?,
                &repo,
                number,
            )?;
            let mut detail = first.detail;
            let (mut files_after, mut threads_after) = (first.files_after, first.threads_after);
            let mut mixed = false;
            while !mixed && (files_after.is_some() || threads_after.is_some()) {
                variables["withFiles"] = files_after.is_some().into();
                variables["filesAfter"] = files_after.into();
                variables["withThreads"] = threads_after.is_some().into();
                variables["threadsAfter"] = threads_after.into();
                let page = parse_pr_page(
                    &self.graphql(&query, variables.clone()).await?,
                    &repo,
                    number,
                )?;
                mixed = !merge_page(&mut detail, page.detail);
                (files_after, threads_after) = (page.files_after, page.threads_after);
            }
            if !mixed {
                return Ok(detail);
            }
        }
        Err(CoreError::Api(format!(
            "{repo}#{number} got new commits while it loaded, twice. Try again."
        )))
    }

    /// Syncs GitHub's per-file "Viewed" checkbox.
    pub async fn set_file_viewed(
        &self,
        pull_request_id: String,
        path: String,
        viewed: bool,
    ) -> Result<(), CoreError> {
        let mutation = if viewed { MARK_VIEWED } else { UNMARK_VIEWED };
        let variables = json!({ "input": { "pullRequestId": pull_request_id, "path": path } });
        parse_mutation(&self.graphql(mutation, variables).await?).map(drop)
    }
}

/// A review GitHub has posted.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SubmitResult {
    pub url: String,
    /// Set when cleaning up here failed afterwards. The review mustn't be submitted again.
    pub warning: Option<String>,
}

/// What the reviewer confirmed in the submit sheet.
pub(crate) struct Submission {
    pub pull_request_id: String,
    pub commit_oid: String,
    pub event: ReviewEvent,
    pub body: String,
    pub drafts: Vec<DraftComment>,
}

/// A submit whose drafts are in a pending review on GitHub, saved next to the
/// drafts before the review is submitted. A retry settles it first, by what
/// GitHub has, so a lost answer never leads to a second review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SubmitAttempt {
    pub pr_node_id: String,
    /// The account that submitted. Only it can settle the record.
    pub viewer_id: String,
    pub viewer_login: String,
    pub review_id: String,
    pub review_url: String,
    pub commit_oid: String,
    pub event: ReviewEvent,
    pub body: String,
    /// The confirmed drafts, as they were sent. One edited since is a different draft.
    pub drafts: Vec<DraftComment>,
}

/// Called with the submit record's path right before phase 2, so live tests can
/// keep the files a lost answer would have left.
#[cfg(test)]
pub(crate) static BEFORE_PHASE_2: Mutex<Option<fn(&Path)>> = Mutex::new(None);

impl GitHubClient {
    /// Submits a review in two steps: the drafts go into a pending review, which
    /// is recorded next to them, then submitted. GitHub is the source of truth,
    /// and nothing is ever deleted there. Retrying after an error is safe: it
    /// never posts the review twice. Once GitHub has the review, the result is
    /// a success, with a warning if cleaning up here failed.
    pub(crate) async fn submit_review(
        &self,
        store: &DraftStore,
        repo: &str,
        number: u64,
        submission: Submission,
    ) -> Result<SubmitResult, CoreError> {
        // One account for the whole submit, even if gh switches midway.
        let github = self.pinned().await?;
        let mut resume = None;
        let viewer = github.viewer().await?;
        // An earlier submit is settled first, by what GitHub has of its review now.
        if let Some(record) = store.attempt(repo, number)? {
            if record.pr_node_id != submission.pull_request_id {
                return Err(CoreError::Storage(format!(
                    "{} is for another pull request. Move it aside to submit here.",
                    store.attempt_path(repo, number)?.display()
                )));
            }
            // Another account can't see that review as its own, or tell a deleted one from a hidden one.
            if record.viewer_id != viewer.id {
                return Err(CoreError::Api(format!(
                    "an earlier submit on this PR was made as {}, but gh is now signed in as {}. \
                     Switch back with `gh auth switch` to settle it, then submit again.",
                    record.viewer_login, viewer.login
                )));
            }
            let remote = github
                .remote_review(&record.review_id)
                .await
                .map_err(|error| {
                    CoreError::Api(format!(
                        "couldn't check the review an earlier submit started ({}). Retrying \
                         is safe: it won't post the review twice.",
                        detail(&error)
                    ))
                })?;
            match (recover(&record, remote.as_ref(), &submission), remote) {
                (Recovery::Forget, _) | (_, None) => store.save_attempt(repo, number, None)?,
                (Recovery::Resume, Some(remote)) => resume = Some((record.review_id, remote)),
                (Recovery::Elsewhere, Some(remote)) => {
                    return Err(CoreError::Api(format!(
                        "your pending review from an earlier submit is on commit {}, but you're \
                         reviewing {}: {}. Submit or delete it on github.com, then try again.",
                        short_oid(&remote.commit_oid),
                        short_oid(&submission.commit_oid),
                        remote.url
                    )));
                }
                (Recovery::Posted, Some(remote)) => {
                    // An earlier try went through as asked; only its answer was lost.
                    let sent = sent_in(&record, &remote);
                    let warning = finish(store, repo, number, &record.drafts, &sent);
                    return Ok(SubmitResult {
                        url: remote.url,
                        warning,
                    });
                }
                (Recovery::PostedDifferently, Some(remote)) => {
                    // What GitHub posted mustn't stay sendable; the rest stays for a new review.
                    let sent = sent_in(&record, &remote);
                    let cleanup = match store.finish_attempt(repo, number, &sent) {
                        Ok(()) => String::new(),
                        Err(error) if record_kept(store, repo, number) => format!(
                            " But removing them here failed ({}); submitting again finishes \
                             that, and won't post them twice.",
                            detail(&error)
                        ),
                        Err(error) => format!(" Saving that here failed ({}).", detail(&error)),
                    };
                    return Err(CoreError::Api(format!(
                        "an earlier submit was already posted as {} on commit {}: {}. This \
                         submit wasn't sent. {}{cleanup}",
                        posted_as(&remote.state),
                        short_oid(&remote.commit_oid),
                        remote.url,
                        stayed(&submission.drafts, &sent)
                    )));
                }
            }
        }

        let (review_id, pending_url) = match resume {
            Some((review_id, remote)) => {
                github.add_threads(&submission, &review_id, &remote).await?;
                (review_id, remote.url)
            }
            None => github.start(&submission).await?,
        };
        let record = SubmitAttempt {
            pr_node_id: submission.pull_request_id,
            viewer_id: viewer.id,
            viewer_login: viewer.login,
            review_id,
            review_url: pending_url,
            commit_oid: submission.commit_oid,
            event: submission.event,
            body: submission.body,
            drafts: submission.drafts,
        };
        // Recorded before the last step: if its answer is lost, a retry finds the review.
        store.save_attempt(repo, number, Some(&record))?;
        #[cfg(test)]
        {
            let hook = *lock(&BEFORE_PHASE_2);
            if let Some(hook) = hook {
                hook(&store.attempt_path(repo, number)?);
            }
        }
        let variables = submit_pending_variables(&record.review_id, record.event, &record.body);
        let url = submitted_url(github.graphql(SUBMIT_PENDING, variables).await)?;
        // GitHub has it now: whatever happens here, it's a success. Only the drafts
        // its review holds are removed; one deleted on github.com meanwhile stays.
        let warning = match github.read_review(&record.review_id).await {
            Ok(Some(remote)) => finish(
                store,
                repo,
                number,
                &record.drafts,
                &sent_in(&record, &remote),
            ),
            other => Some(format!(
                "Your review was posted, but checking which drafts it holds failed ({}), so \
                 they're kept here. Submit again to finish: it won't post twice.",
                other
                    .err()
                    .map_or("GitHub didn't find it".into(), |e| detail(&e))
            )),
        };
        Ok(SubmitResult { url, warning })
    }

    /// Phase 1 without a record: puts the drafts in the viewer's pending review
    /// on the submission's commit, one from github.com or a new one. Returns its
    /// id and URL.
    async fn start(&self, submission: &Submission) -> Result<(String, String), CoreError> {
        let lookup = format!("{PENDING_LOOKUP}{PENDING_REVIEWS}");
        let pending = parse_pending_lookup(
            &self
                .graphql(&lookup, json!({ "id": submission.pull_request_id }))
                .await?,
        )?;
        if let Some(pending) = pending {
            // Every comment it has, not just the first page the lookup reads, and its commit for sure.
            let remote = self.remote_review(&pending.id).await?.ok_or_else(|| {
                CoreError::Api(
                    "your pending review on github.com was just deleted. Try again.".into(),
                )
            })?;
            check_pending_commit(&remote.commit_oid, &submission.commit_oid)?;
            self.add_threads(submission, &pending.id, &remote).await?;
            return Ok((pending.id, remote.url));
        }
        let variables = create_pending_variables(
            &submission.pull_request_id,
            &submission.commit_oid,
            &submission.drafts,
        );
        let data = parse_mutation(&self.graphql(CREATE_PENDING, variables).await?)?;
        let field = |name: &str| {
            data.pointer(&format!("/addPullRequestReview/pullRequestReview/{name}"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| CoreError::Api(format!("GitHub's response had no review {name}")))
        };
        Ok((field("id")?, field("url")?))
    }

    /// Adds the drafts `review`, the pending review on the submission's commit, lacks.
    async fn add_threads(
        &self,
        submission: &Submission,
        review_id: &str,
        review: &RemoteReview,
    ) -> Result<(), CoreError> {
        let new = missing(&submission.drafts, &review.comments);
        if !new.is_empty() {
            // shortcut: a push between this read and the mutations still moves the drafts; GitHub takes no commit for them.
            let head = self.head_oid(&submission.pull_request_id).await?;
            check_threads_land(&head, &submission.commit_oid, &review.url)?;
        }
        // Retrying after a failure is safe: drafts already in the pending review are skipped.
        let partial = |added: usize, error: CoreError| {
            CoreError::Api(format!(
                "added {added} of {} comments to your pending review on GitHub, then failed: {}. \
                 Retrying is safe: comments already added are skipped.",
                new.len(),
                detail(&error)
            ))
        };
        for (added, draft) in new.iter().enumerate() {
            let result = self
                .graphql(ADD_THREAD, add_thread_variables(review_id, draft))
                .await
                .and_then(|response| parse_mutation(&response))
                .and_then(|data| {
                    // GitHub can return no thread without an error, e.g. for a line outside the diff.
                    match data.pointer("/addPullRequestReviewThread/thread") {
                        Some(Value::Object(_)) => Ok(()),
                        _ => Err(CoreError::Api(format!(
                            "GitHub didn't accept the comment on {} line {}",
                            draft.path, draft.line
                        ))),
                    }
                });
            result.map_err(|error| partial(added, error))?;
        }
        Ok(())
    }

    /// What a discarded submit leaves pending on github.com, as far as GitHub says.
    pub(crate) async fn left_on_github(&self, record: &SubmitAttempt) -> Option<LeftOnGitHub> {
        // One account for the check and the lookup.
        let lookup = match self.pinned().await {
            Ok(github) => match github.viewer().await {
                Ok(viewer) if viewer.id == record.viewer_id => {
                    github.read_review(&record.review_id).await
                }
                // Another account can't see that review, so GitHub can't say.
                Ok(viewer) => Err(CoreError::Api(format!("signed in as {}", viewer.login))),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        what_is_left(&lookup, &record.review_url)
    }

    async fn head_oid(&self, pull_request_id: &str) -> Result<String, CoreError> {
        let response = self
            .graphql(PR_HEAD, json!({ "id": pull_request_id }))
            .await?;
        let (data, messages) = split_response(&response)?;
        match data.pointer("/node/headRefOid").and_then(Value::as_str) {
            Some(head) if messages.is_empty() => Ok(head.to_owned()),
            _ => Err(api_error(messages)),
        }
    }

    async fn viewer(&self) -> Result<Account, CoreError> {
        let (mut data, messages) = split_response(&self.graphql(VIEWER, json!({})).await?)?;
        let viewer = data
            .pointer_mut("/viewer")
            .map(Value::take)
            .unwrap_or_default();
        if !messages.is_empty() || viewer.is_null() {
            return Err(api_error(messages));
        }
        serde_json::from_value(viewer).map_err(unexpected)
    }

    /// A review by id with all its comments and its commit. `None` only when
    /// GitHub says the node isn't found, which for a review means it was deleted
    /// while pending.
    pub(crate) async fn remote_review(
        &self,
        review_id: &str,
    ) -> Result<Option<RemoteReview>, CoreError> {
        let review = self.read_review(review_id).await?;
        // GitHub can leave a review's commit out for a moment, e.g. right after a
        // submit. That's unknown, not another commit.
        if !review
            .as_ref()
            .is_some_and(|review| review.commit_oid.is_empty())
        {
            return Ok(review);
        }
        tokio::time::sleep(COMMIT_REREAD).await;
        known_commit(self.read_review(review_id).await?)
    }

    /// `remote_review` without waiting for a commit GitHub left out, for reads
    /// that only need the state or the comments.
    async fn read_review(&self, review_id: &str) -> Result<Option<RemoteReview>, CoreError> {
        let mut review: Option<RemoteReview> = None;
        let mut after: Option<String> = None;
        loop {
            let variables = json!({ "id": review_id, "after": after });
            let Some((page, next)) =
                parse_review_page(&self.graphql(REMOTE_REVIEW, variables).await?)?
            else {
                return Ok(None);
            };
            match &mut review {
                Some(review) => review.comments.extend(page.comments),
                None => review = Some(page),
            }
            if next.is_none() {
                return Ok(review);
            }
            after = next;
        }
    }
}

#[derive(Debug, Deserialize)]
struct Account {
    id: String,
    login: String,
}

/// A review as GitHub has it now.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RemoteReview {
    /// PENDING until submitted; then COMMENTED, APPROVED, CHANGES_REQUESTED or DISMISSED.
    pub state: String,
    pub viewer_did_author: bool,
    pub url: String,
    pub body: String,
    /// Never empty from `remote_review`, which fails instead. `read_review` may
    /// leave it empty.
    pub commit_oid: String,
    pub comments: Vec<PendingComment>,
}

/// How long to wait before reading a review again whose commit GitHub left out.
const COMMIT_REREAD: std::time::Duration = std::time::Duration::from_secs(2);

/// A review whose commit is still unknown can't be settled yet; trying again later is safe.
fn known_commit(review: Option<RemoteReview>) -> Result<Option<RemoteReview>, CoreError> {
    match review {
        Some(review) if review.commit_oid.is_empty() => Err(CoreError::Api(
            "GitHub didn't say which commit your review is on. Try again in a moment: it \
             won't post the review twice."
                .into(),
        )),
        review => Ok(review),
    }
}

/// What a discarded submit's review is on github.com.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum LeftOnGitHub {
    /// Still pending there, for you to submit or delete.
    Pending { url: String },
    /// GitHub couldn't be asked, so it may still be pending.
    Unknown { url: String },
}

/// What a looked-up review leaves on github.com: nothing once it's submitted or gone.
fn what_is_left(
    lookup: &Result<Option<RemoteReview>, CoreError>,
    url: &str,
) -> Option<LeftOnGitHub> {
    let url = url.to_owned();
    match lookup {
        Ok(Some(review)) if review.state == "PENDING" => Some(LeftOnGitHub::Pending { url }),
        Ok(_) => None,
        Err(_) => Some(LeftOnGitHub::Unknown { url }),
    }
}

/// What an earlier submit's review, as GitHub has it now, means for this one.
#[derive(Debug, PartialEq)]
enum Recovery {
    /// Deleted while pending: forget the record and start over.
    Forget,
    /// Pending on this submit's commit: add what it lacks, then submit it.
    Resume,
    /// Pending on another commit: it's left for the reviewer to submit or delete.
    Elsewhere,
    /// Posted exactly as this submit asks; only the answer was lost.
    Posted,
    /// Posted, but not as this submit asks, so this submit isn't sent.
    PostedDifferently,
}

fn recover(
    record: &SubmitAttempt,
    remote: Option<&RemoteReview>,
    submission: &Submission,
) -> Recovery {
    let Some(remote) = remote else {
        return Recovery::Forget;
    };
    if remote.state == "PENDING" {
        return if remote.commit_oid == submission.commit_oid {
            Recovery::Resume
        } else {
            Recovery::Elsewhere
        };
    }
    let same_drafts = record.drafts.len() == submission.drafts.len()
        && submission
            .drafts
            .iter()
            .all(|draft| record.drafts.contains(draft));
    let exact = remote.viewer_did_author
        && remote.commit_oid == submission.commit_oid
        && posted_event(&remote.state) == Some(submission.event)
        && remote.body.trim() == submission.body.trim()
        && same_drafts
        // What GitHub posted decides, not the record.
        && missing(&submission.drafts, &remote.comments).is_empty();
    if exact {
        Recovery::Posted
    } else {
        Recovery::PostedDifferently
    }
}

/// The event a submitted review's state came from. A dismissed one doesn't say.
fn posted_event(state: &str) -> Option<ReviewEvent> {
    match state {
        "APPROVED" => Some(ReviewEvent::Approve),
        "CHANGES_REQUESTED" => Some(ReviewEvent::RequestChanges),
        "COMMENTED" => Some(ReviewEvent::Comment),
        _ => None,
    }
}

/// A submitted review's state, as the verdict it carries.
fn posted_as(state: &str) -> &str {
    match state {
        "APPROVED" => "an approval",
        "CHANGES_REQUESTED" => "a request for changes",
        "COMMENTED" => "a comment",
        "DISMISSED" => "a review that has since been dismissed",
        other => other,
    }
}

/// The recorded drafts a submitted review holds: yours, if the review is.
fn sent_in(record: &SubmitAttempt, remote: &RemoteReview) -> Vec<DraftComment> {
    if !remote.viewer_did_author {
        return Vec::new();
    }
    present(&record.drafts, &remote.comments)
}

/// Which of `drafts` are still here after the ones GitHub posted were removed.
fn stayed(drafts: &[DraftComment], sent: &[DraftComment]) -> String {
    let left: Vec<String> = drafts
        .iter()
        .filter(|draft| !sent.contains(draft))
        .map(|draft| format!("{}:{}", draft.path, draft.line))
        .collect();
    if left.is_empty() {
        "Its drafts were all in it, so they're removed here.".into()
    } else {
        format!("Drafts not in it are still here: {}.", left.join(", "))
    }
}

/// Settles a submit GitHub posted: removes the `recorded` drafts its review
/// holds (`sent`), then the record. Returns a warning for recorded drafts it
/// doesn't hold, which stay, and for a cleanup that failed, worded by what's left.
fn finish(
    store: &DraftStore,
    repo: &str,
    number: u64,
    recorded: &[DraftComment],
    sent: &[DraftComment],
) -> Option<String> {
    let mut warnings = Vec::new();
    let left_out: Vec<String> = recorded
        .iter()
        .filter(|draft| !sent.contains(draft))
        .map(|draft| format!("{}:{}", draft.path, draft.line))
        .collect();
    if !left_out.is_empty() {
        warnings.push(format!(
            "Your review was posted without {} of its drafts, maybe deleted on github.com \
             meanwhile. They're still here: {}.",
            left_out.len(),
            left_out.join(", ")
        ));
    }
    if let Err(error) = store.finish_attempt(repo, number, sent) {
        warnings.push(if record_kept(store, repo, number) {
            format!(
                "Your review was posted, but cleaning up here failed ({}). Submit again to \
                 finish: it won't post twice.",
                detail(&error)
            )
        } else {
            format!(
                "Your review was posted and its drafts removed, but saving that here failed \
                 ({}). Don't submit it again.",
                detail(&error)
            )
        });
    }
    (!warnings.is_empty()).then(|| warnings.join(" "))
}

/// Whether the submit record is still here: then submitting again settles it
/// by what GitHub has, and can't post twice.
fn record_kept(store: &DraftStore, repo: &str, number: u64) -> bool {
    matches!(store.attempt(repo, number), Ok(Some(_)))
}

/// An error's text without the "GitHub API error:" its own message would add again.
fn detail(error: &CoreError) -> String {
    match error {
        CoreError::Api(message) => message.clone(),
        other => other.to_string(),
    }
}

/// The review's URL from `SUBMIT_PENDING`'s answer, or why there's none. Typed
/// GraphQL errors without a review are GitHub refusing it, so nothing was
/// posted. Anything else may have been, and a retry settles it.
fn submitted_url(response: Result<String, CoreError>) -> Result<String, CoreError> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(default)]
        data: Value,
        #[serde(default)]
        errors: Vec<TypedError>,
    }
    let unknown = |why: String| {
        CoreError::Api(format!(
            "submitting your review failed ({why}). Retrying is safe: it won't post the \
             review twice."
        ))
    };
    let body = response.map_err(|error| unknown(detail(&error)))?;
    let response: Response = serde_json::from_str(&body)
        .map_err(|error| unknown(format!("unexpected response: {error}")))?;
    let url = response
        .data
        .pointer("/submitPullRequestReview/pullRequestReview/url")
        .and_then(Value::as_str);
    let messages: Vec<&str> = response.errors.iter().map(|e| e.message.as_str()).collect();
    match url {
        Some(url) if messages.is_empty() => Ok(url.to_owned()),
        None if !messages.is_empty() && response.errors.iter().all(|e| !e.kind.is_empty()) => {
            Err(CoreError::Api(format!(
                "GitHub refused the review: {}. Nothing was posted, and your pending review is \
                 kept on github.com. Change the verdict or fix the cause, then submit again.",
                messages.join("; ")
            )))
        }
        _ if messages.is_empty() => Err(unknown("GitHub's response had no review URL".into())),
        _ => Err(unknown(messages.join("; "))),
    }
}

/// A GraphQL error with GitHub's `type`, e.g. NOT_FOUND. Errors like timeouts have none.
#[derive(Deserialize)]
struct TypedError {
    #[serde(rename = "type", default)]
    kind: String,
    message: String,
    #[serde(default)]
    path: Vec<Value>,
}

/// Parses a page of `REMOTE_REVIEW`: the review and the cursor of the next page
/// of comments. `None` only for GitHub's NOT_FOUND on the node itself; any other
/// error fails, since the review may still exist.
fn parse_review_page(body: &str) -> Result<Option<(RemoteReview, Option<String>)>, CoreError> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(default)]
        data: Value,
        #[serde(default)]
        errors: Vec<TypedError>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Node {
        state: String,
        url: String,
        body: String,
        viewer_did_author: bool,
        commit: Option<CommitOid>,
        comments: Connection<PendingCommentNode>,
    }
    let response: Response = serde_json::from_str(body).map_err(unexpected)?;
    let node = response.data.get("node").cloned().unwrap_or_default();
    let not_found = |error: &TypedError| error.kind == "NOT_FOUND" && error.path == [json!("node")];
    if node.is_null() && !response.errors.is_empty() && response.errors.iter().all(not_found) {
        return Ok(None);
    }
    if !response.errors.is_empty() {
        return Err(api_error(
            response.errors.into_iter().map(|e| e.message).collect(),
        ));
    }
    if node.is_null() {
        return Err(CoreError::Api(
            "GitHub returned no review for its id".into(),
        ));
    }
    let node: Node = serde_json::from_value(node).map_err(unexpected)?;
    let (comments, next) = node.comments.into_parts();
    Ok(Some((
        RemoteReview {
            state: node.state,
            viewer_did_author: node.viewer_did_author,
            url: node.url,
            body: node.body,
            commit_oid: node.commit.map(|commit| commit.oid).unwrap_or_default(),
            comments: comments.into_iter().map(pending_comment).collect(),
        },
        next,
    )))
}

fn event_name(event: ReviewEvent) -> &'static str {
    match event {
        ReviewEvent::Approve => "APPROVE",
        ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        ReviewEvent::Comment => "COMMENT",
    }
}

/// A draft's location and body, as both thread inputs take them.
fn thread_input(draft: &DraftComment) -> Value {
    let mut thread = json!({
        "path": draft.path, "line": draft.line, "side": draft.side, "body": draft.body,
    });
    if let Some(start_line) = draft.start_line {
        thread["startLine"] = start_line.into();
    }
    if let Some(start_side) = draft.start_side {
        thread["startSide"] = json!(start_side);
    }
    thread
}

/// Variables for `CREATE_PENDING`: every draft becomes a `DraftPullRequestReviewThread`.
/// No event, so the review stays pending; the body is sent when it's submitted.
fn create_pending_variables(
    pull_request_id: &str,
    commit_oid: &str,
    comments: &[DraftComment],
) -> Value {
    let threads: Vec<Value> = comments.iter().map(thread_input).collect();
    json!({ "input": {
        "pullRequestId": pull_request_id, "commitOID": commit_oid, "threads": threads,
    } })
}

fn add_thread_variables(pending_review_id: &str, draft: &DraftComment) -> Value {
    let mut input = thread_input(draft);
    input["pullRequestReviewId"] = pending_review_id.into();
    json!({ "input": input })
}

fn submit_pending_variables(pending_review_id: &str, event: ReviewEvent, body: &str) -> Value {
    json!({ "input": {
        "pullRequestReviewId": pending_review_id, "event": event_name(event), "body": body,
    } })
}

/// A pending review is submitted on the commit it was started on, so for any
/// verdict, even an approval with no new comments, that must be the one under review.
fn check_pending_commit(pending_commit: &str, commit_oid: &str) -> Result<(), CoreError> {
    if pending_commit == commit_oid {
        return Ok(());
    }
    Err(CoreError::Api(format!(
        "your pending review on github.com is on commit {}, but you're reviewing {}. \
         Submit or cancel it on github.com, then try again.",
        short_oid(pending_commit),
        short_oid(commit_oid)
    )))
}

/// `addPullRequestReviewThread` puts a thread on the PR's head, whatever commit
/// its pending review is on, so drafts written on an older commit would land on
/// other lines. They're only added while the head is still their commit.
fn check_threads_land(head: &str, commit_oid: &str, review_url: &str) -> Result<(), CoreError> {
    if head == commit_oid {
        return Ok(());
    }
    Err(CoreError::Api(format!(
        "your pending review is on commit {}, and the PR has newer commits. GitHub would put \
         your drafts on the newest commit's lines, so none were added. Submit or delete that \
         review on github.com, then submit again: {review_url}",
        short_oid(commit_oid)
    )))
}

/// Pairs each draft with a review comment that is it, one to one: two identical
/// drafts need two comments. They match on path, original range (which later
/// pushes don't move, so the current one is only a fallback) and trimmed text,
/// and on side where the comment's is known. Returns whether each draft found
/// its comment.
fn find_in(drafts: &[DraftComment], comments: &[PendingComment]) -> Vec<bool> {
    // A single-line range is sent without a start line, whichever way it was stored.
    let start = |start: Option<u32>, line: Option<u32>| start.filter(|&s| Some(s) != line);
    let same = |draft: &DraftComment, comment: &PendingComment| {
        let line = comment.original_line.or(comment.line);
        comment.path == draft.path
            && line == Some(draft.line)
            && start(comment.original_start_line.or(comment.start_line), line)
                == start(draft.start_line, Some(draft.line))
            && comment.body.trim() == draft.body.trim()
            && comment.side.is_none_or(|side| side == draft.side)
    };
    let mut used = vec![false; comments.len()];
    let mut found = vec![false; drafts.len()];
    // Comments whose side is known first, so a draft that matches one leaves a
    // context line's comment for a draft on the other side.
    for known in [true, false] {
        for (draft, found) in drafts.iter().zip(&mut found) {
            let free = |(i, comment): &(usize, &PendingComment)| {
                !used[*i] && comment.side.is_some() == known && same(draft, comment)
            };
            if let (false, Some((i, _))) = (*found, comments.iter().enumerate().find(free)) {
                used[i] = true;
                *found = true;
            }
        }
    }
    found
}

/// Drafts the review doesn't have yet, so a retry doesn't post them twice.
fn missing<'a>(drafts: &'a [DraftComment], comments: &[PendingComment]) -> Vec<&'a DraftComment> {
    let found = find_in(drafts, comments);
    drafts
        .iter()
        .zip(found)
        .filter(|(_, found)| !found)
        .map(|(draft, _)| draft)
        .collect()
}

/// Drafts the review has.
fn present(drafts: &[DraftComment], comments: &[PendingComment]) -> Vec<DraftComment> {
    let found = find_in(drafts, comments);
    drafts
        .iter()
        .zip(found)
        .filter(|(_, found)| *found)
        .map(|(draft, _)| draft.clone())
        .collect()
}

/// The first 7 characters, as GitHub shows commits.
fn short_oid(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

/// Splits "owner/name". Only characters GitHub allows in names pass, so the
/// parts are also safe as path components.
fn split_repo(repo: &str) -> Option<(&str, &str)> {
    let (owner, name) = repo.split_once('/')?;
    let valid = |part: &str| {
        !matches!(part, "" | "." | "..")
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    (valid(owner) && valid(name)).then_some((owner, name))
}

// --- GraphQL responses --------------------------------------------------------

/// Splits a GraphQL body into its `data` (null when absent) and GitHub's error messages.
fn split_response(body: &str) -> Result<(Value, Vec<String>), CoreError> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(default)]
        data: Value,
        errors: Option<Vec<GraphQlError>>,
    }
    let response: Response = serde_json::from_str(body).map_err(unexpected)?;
    let messages = response
        .errors
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.message)
        .collect();
    Ok((response.data, messages))
}

fn unexpected(error: serde_json::Error) -> CoreError {
    CoreError::Api(format!("unexpected response: {error}"))
}

fn api_error(messages: Vec<String>) -> CoreError {
    if messages.is_empty() {
        return CoreError::Api("response contained no data".into());
    }
    CoreError::Api(messages.join("; "))
}

/// For a mutation any GraphQL error is a failure, since part of it may not have applied.
fn parse_mutation(body: &str) -> Result<Value, CoreError> {
    let (data, messages) = split_response(body)?;
    if messages.is_empty() && !data.is_null() {
        return Ok(data);
    }
    // addPullRequestReview can't create a second review while one is pending.
    // prepare_review checks first, so this means one was started on github.com meanwhile.
    if messages
        .iter()
        .any(|m| m.to_lowercase().contains("one pending review"))
    {
        return Err(CoreError::Api(format!(
            "you just started a pending review on this PR on github.com. Try again to add your drafts to it ({})",
            messages.join("; ")
        )));
    }
    Err(api_error(messages))
}

#[derive(Debug)]
struct PrPage {
    detail: PullRequestDetail,
    /// Cursor for the next page, `None` when the connection is done or wasn't requested.
    files_after: Option<String>,
    threads_after: Option<String>,
}

fn parse_pr_page(body: &str, repo: &str, number: u64) -> Result<PrPage, CoreError> {
    let (mut data, messages) = split_response(body)?;
    if data.is_null() {
        return Err(api_error(messages));
    }
    let pr = data
        .pointer_mut("/repository/pullRequest")
        .map(Value::take)
        .unwrap_or_default();
    if pr.is_null() {
        // A PR that's missing or hidden from you comes with NOT_FOUND. Other
        // errors, e.g. a timeout, say why there's no PR this time.
        #[derive(Deserialize)]
        struct Errors {
            #[serde(default)]
            errors: Vec<TypedError>,
        }
        let errors: Errors = serde_json::from_str(body).map_err(unexpected)?;
        if errors.errors.iter().any(|error| error.kind != "NOT_FOUND") {
            return Err(api_error(messages));
        }
        return Err(CoreError::Api(format!(
            "{repo}#{number} wasn't found, or your gh account can't see it"
        )));
    }
    // Fail rather than show a partial file or thread list: the detail has no
    // way to warn, and reviewing with threads silently missing would mislead.
    if !messages.is_empty() {
        return Err(api_error(messages));
    }

    let node: PrNode = serde_json::from_value(pr).map_err(unexpected)?;
    let (files, files_after) = node.files.map(Connection::into_parts).unwrap_or_default();
    let (threads, threads_after) = node
        .review_threads
        .map(Connection::into_parts)
        .unwrap_or_default();
    Ok(PrPage {
        detail: PullRequestDetail {
            id: node.id,
            repo: repo.to_owned(),
            number,
            title: node.title,
            body: node.body,
            url: node.url,
            author: node.author.map(|author| author.login),
            is_draft: node.is_draft,
            viewer_did_author: node.viewer_did_author,
            base_ref_name: node.base_ref_name,
            head_ref_name: node.head_ref_name,
            head_oid: node.head_ref_oid,
            files,
            threads: threads
                .into_iter()
                .filter_map(|thread| convert_thread(thread).transpose())
                .collect::<Result<_, _>>()?,
            pending_review: viewer_pending_review(node.reviews),
            failed_checks: failed_checks(node.commits),
            last_reviewed_oid: last_reviewed_oid(node.last_reviews),
        },
        files_after,
        threads_after,
    })
}

/// Adds a later page of a PR to its first. False if the page is on another head,
/// after a push between them: its thread lines refer to another diff.
fn merge_page(detail: &mut PullRequestDetail, page: PullRequestDetail) -> bool {
    if page.head_oid != detail.head_oid {
        return false;
    }
    detail.files.extend(page.files);
    detail.threads.extend(page.threads);
    true
}

/// Parses `PENDING_LOOKUP`. Any error fails, since a mutation follows.
fn parse_pending_lookup(body: &str) -> Result<Option<PendingReview>, CoreError> {
    #[derive(Deserialize)]
    struct PullRequestNode {
        reviews: Option<Connection<ReviewNode>>,
    }
    let (mut data, messages) = split_response(body)?;
    if !messages.is_empty() {
        return Err(api_error(messages));
    }
    let node = data
        .pointer_mut("/node")
        .map(Value::take)
        .unwrap_or_default();
    if node.is_null() {
        return Err(CoreError::Api(
            "the pull request wasn't found, or your gh account can't see it".into(),
        ));
    }
    let node: PullRequestNode = serde_json::from_value(node).map_err(unexpected)?;
    Ok(viewer_pending_review(node.reviews))
}

fn viewer_pending_review(reviews: Option<Connection<ReviewNode>>) -> Option<PendingReview> {
    let review = reviews?
        .nodes
        .into_iter()
        .find(|review| review.viewer_did_author)?;
    Some(pending_review(review))
}

fn pending_review(review: ReviewNode) -> PendingReview {
    // shortcut: the window shows only the first 100 comments of a pending review; submitting reads them all.
    PendingReview {
        id: review.id,
        commit_oid: review.commit.map(|commit| commit.oid).unwrap_or_default(),
        comments: review
            .comments
            .nodes
            .into_iter()
            .map(pending_comment)
            .collect(),
    }
}

fn pending_comment(comment: PendingCommentNode) -> PendingComment {
    // The hunk ends at the commented line: a removed line is on the left, an
    // added one on the right, and a context line on either.
    let side = match comment
        .diff_hunk
        .lines()
        .last()
        .and_then(|line| line.chars().next())
    {
        Some('-') => Some(DiffSide::Left),
        Some('+') => Some(DiffSide::Right),
        _ => None,
    };
    PendingComment {
        path: comment.path,
        line: comment.line,
        start_line: comment.start_line,
        original_line: comment.original_line,
        original_start_line: comment.original_start_line,
        side,
        body: comment.body,
    }
}

/// Leaves out the viewer's pending comments, which `pending_review` already
/// shows, and returns `None` for a thread made only of them.
fn convert_thread(node: ThreadNode) -> Result<Option<ReviewThread>, CoreError> {
    let mut nodes = node.comments.nodes;
    let had_comments = !nodes.is_empty();
    nodes.retain(|comment| comment.state != "PENDING");
    if had_comments && nodes.is_empty() {
        return Ok(None);
    }
    let commit_oid = nodes
        .first()
        .and_then(|comment| comment.commit.as_ref())
        .map(|commit| commit.oid.clone());
    let comments = nodes
        .into_iter()
        .map(|comment| {
            let created_at = humantime::parse_rfc3339(&comment.created_at).map_err(|e| {
                CoreError::Api(format!("bad createdAt {:?}: {e}", comment.created_at))
            })?;
            Ok(ThreadComment {
                author: comment.author.map(|author| author.login),
                body: comment.body,
                created_at,
                url: comment.url,
            })
        })
        .collect::<Result<_, CoreError>>()?;
    Ok(Some(ReviewThread {
        id: node.id,
        path: node.path,
        line: node.line,
        start_line: node.start_line,
        side: node.diff_side,
        start_side: node.start_diff_side,
        is_resolved: node.is_resolved,
        is_outdated: node.is_outdated,
        commit_oid,
        comments,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    id: String,
    title: String,
    body: String,
    url: String,
    is_draft: bool,
    viewer_did_author: bool,
    base_ref_name: String,
    head_ref_name: String,
    head_ref_oid: String,
    author: Option<Author>,
    // Absent when skipped with `@include`; `files` can also be null on GitHub's side.
    #[serde(default)]
    files: Option<Connection<ChangedFile>>,
    #[serde(default)]
    review_threads: Option<Connection<ThreadNode>>,
    reviews: Option<Connection<ReviewNode>>,
    #[serde(default)]
    last_reviews: Option<OptionalNodes<MyReviewNode>>,
    #[serde(default)]
    commits: Option<OptionalNodes<ChecksCommitNode>>,
}

/// A connection whose nodes GitHub may return as null.
#[derive(Deserialize)]
struct OptionalNodes<T> {
    nodes: Vec<Option<T>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MyReviewNode {
    viewer_did_author: bool,
    commit: Option<MyReviewCommit>,
}

#[derive(Deserialize)]
struct MyReviewCommit {
    oid: String,
}

#[derive(Deserialize)]
struct ChecksCommitNode {
    commit: ChecksCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChecksCommit {
    status_check_rollup: Option<ChecksRollup>,
}

#[derive(Deserialize)]
struct ChecksRollup {
    contexts: OptionalNodes<CheckContext>,
}

/// A check run or a commit status; each fills in its own half.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckContext {
    name: Option<String>,
    conclusion: Option<String>,
    details_url: Option<String>,
    context: Option<String>,
    state: Option<String>,
    target_url: Option<String>,
}

fn failed_checks(commits: Option<OptionalNodes<ChecksCommitNode>>) -> Vec<FailedCheck> {
    let contexts = commits
        .and_then(|commits| commits.nodes.into_iter().flatten().next())
        .and_then(|node| node.commit.status_check_rollup)
        .map(|rollup| rollup.contexts.nodes)
        .unwrap_or_default();
    contexts
        .into_iter()
        .flatten()
        .filter_map(|check| {
            let failed = matches!(
                check.conclusion.as_deref(),
                Some("FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED")
            ) || matches!(check.state.as_deref(), Some("FAILURE" | "ERROR"));
            let name = check.name.or(check.context)?;
            failed.then_some(FailedCheck { name, url: check.details_url.or(check.target_url) })
        })
        .collect()
}

fn last_reviewed_oid(reviews: Option<OptionalNodes<MyReviewNode>>) -> Option<String> {
    let mine = reviews?.nodes.into_iter().flatten().rev().find(|review| review.viewer_did_author)?;
    mine.commit.map(|commit| commit.oid)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewNode {
    id: String,
    viewer_did_author: bool,
    commit: Option<CommitOid>,
    comments: Connection<PendingCommentNode>,
}

#[derive(Deserialize)]
struct CommitOid {
    oid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingCommentNode {
    path: String,
    line: Option<u32>,
    start_line: Option<u32>,
    original_line: Option<u32>,
    original_start_line: Option<u32>,
    body: String,
    diff_hunk: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    // Comment lists are fetched without pageInfo.
    #[serde(default)]
    page_info: PageInfo,
    nodes: Vec<T>,
}

impl<T> Connection<T> {
    fn into_parts(self) -> (Vec<T>, Option<String>) {
        let next = self
            .page_info
            .end_cursor
            .filter(|_| self.page_info.has_next_page);
        (self.nodes, next)
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadNode {
    id: String,
    path: String,
    line: Option<u32>,
    start_line: Option<u32>,
    diff_side: DiffSide,
    start_diff_side: Option<DiffSide>,
    is_resolved: bool,
    is_outdated: bool,
    // shortcut: only a thread's first 100 comments are fetched, paginate them once threads that long need to show in full.
    comments: Connection<CommentNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentNode {
    author: Option<Author>,
    body: String,
    created_at: String,
    url: String,
    /// PENDING or SUBMITTED. Kept as a string so a new state doesn't break parsing.
    state: String,
    #[serde(default)]
    commit: Option<CommitOid>,
}

// --- Drafts ---------------------------------------------------------------------

/// Notified after a PR's drafts change, from whichever thread changed them.
#[uniffi::export(with_foreign)]
pub trait DraftListener: Send + Sync {
    fn drafts_changed(&self, repo: String, number: u64);
}

/// Unsent review comments, one JSON file per PR, shared by the UI and the
/// agents' MCP server. Use one store per `root`: the cache assumes nothing
/// else writes the files.
#[derive(uniffi::Object)]
pub struct DraftStore {
    root: PathBuf,
    /// Keyed by the PR's draft file, loaded on first access.
    cache: Mutex<HashMap<PathBuf, Vec<DraftComment>>>,
    listener: Mutex<Option<Arc<dyn DraftListener>>>,
}

#[uniffi::export]
impl DraftStore {
    /// `root` is the app's data dir; drafts live in <root>/drafts/<owner>/<name>/<number>.json.
    #[uniffi::constructor]
    pub fn new(root: String) -> Self {
        Self {
            root: root.into(),
            cache: Mutex::default(),
            listener: Mutex::default(),
        }
    }

    pub fn set_listener(&self, listener: Arc<dyn DraftListener>) {
        *lock(&self.listener) = Some(listener);
    }

    pub fn list(&self, repo: String, number: u64) -> Result<Vec<DraftComment>, CoreError> {
        let path = self.path(&repo, number)?;
        Ok(cached(&mut lock(&self.cache), &path)?.clone())
    }

    /// The commit the PR's drafts were written against, `None` without drafts.
    /// The app keeps the diff on it and submits with it.
    pub fn pinned_commit(&self, repo: String, number: u64) -> Result<Option<String>, CoreError> {
        let path = self.path(&repo, number)?;
        let cache = &mut lock(&self.cache);
        Ok(cached(cache, &path)?
            .first()
            .map(|draft| draft.commit_oid.clone()))
    }

    /// Rejects a draft on a different commit than the PR's other drafts.
    pub fn add(
        &self,
        repo: String,
        number: u64,
        draft: NewDraft,
    ) -> Result<DraftComment, CoreError> {
        let draft = DraftComment {
            id: new_id(),
            path: draft.path,
            line: draft.line,
            side: draft.side,
            start_line: draft.start_line,
            start_side: draft.start_side,
            body: draft.body,
            author: draft.author,
            commit_oid: draft.commit_oid,
        };
        self.change(&repo, number, |drafts| {
            // Lines only mean something on one diff, and a review is submitted on one commit.
            if let Some(pinned) = drafts.first().map(|first| &first.commit_oid)
                && *pinned != draft.commit_oid
            {
                return Err(CoreError::Storage(format!(
                    "{repo}#{number}'s drafts are pinned to commit {}, but this comment is on {}. \
                     Submit or discard them to comment on another commit.",
                    short_oid(pinned),
                    short_oid(&draft.commit_oid)
                )));
            }
            drafts.push(draft.clone());
            Ok(())
        })?;
        Ok(draft)
    }

    pub fn update_body(
        &self,
        repo: String,
        number: u64,
        id: String,
        body: String,
    ) -> Result<(), CoreError> {
        self.change(&repo, number, |drafts| {
            let index = position(drafts, &id)?;
            drafts[index].body = body;
            Ok(())
        })
    }

    pub fn remove(&self, repo: String, number: u64, id: String) -> Result<(), CoreError> {
        self.change(&repo, number, |drafts| {
            drafts.remove(position(drafts, &id)?);
            Ok(())
        })
    }

    /// A submit that put drafts in your pending review on GitHub and hasn't been
    /// settled. Editing those drafts here wouldn't change what's on GitHub.
    pub fn unsettled(
        &self,
        repo: String,
        number: u64,
    ) -> Result<Option<UnsettledSubmit>, CoreError> {
        Ok(self.attempt(&repo, number)?.map(|attempt| UnsettledSubmit {
            review_url: attempt.review_url,
            draft_ids: attempt.drafts.into_iter().map(|draft| draft.id).collect(),
        }))
    }

    /// Deletes the PR's drafts, then the record of an unsettled submit. A pending
    /// review that submit left on github.com stays there; nothing is deleted on GitHub.
    pub fn clear(&self, repo: String, number: u64) -> Result<(), CoreError> {
        // Drafts first: if the record can't go, it still guards against posting twice.
        self.change(&repo, number, |drafts| {
            drafts.clear();
            Ok(())
        })?;
        write_file(&self.root, &self.attempt_path(&repo, number)?, None)
    }
}

impl DraftStore {
    /// The drafts with `ids`, as they are now. One added since, e.g. by an agent
    /// after the reviewer confirmed, isn't among them and stays local.
    pub(crate) fn confirmed(
        &self,
        repo: &str,
        number: u64,
        ids: &[String],
    ) -> Result<Vec<DraftComment>, CoreError> {
        let drafts = self.list(repo.to_owned(), number)?;
        ids.iter()
            .map(|id| {
                drafts.iter().find(|draft| &draft.id == id).cloned().ok_or_else(|| {
                    CoreError::Storage(
                        "a draft you were about to submit was removed. Check the list and submit again."
                            .into(),
                    )
                })
            })
            .collect()
    }

    /// The PR's unfinished submit, if a try stopped before it was settled.
    pub(crate) fn attempt(
        &self,
        repo: &str,
        number: u64,
    ) -> Result<Option<SubmitAttempt>, CoreError> {
        let path = self.attempt_path(repo, number)?;
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                CoreError::Storage(format!(
                    "submit record {} is corrupt ({e}). It names a review that may already be on GitHub: \
                     check there, then move the file aside to submit again.",
                    path.display()
                ))
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CoreError::Storage(format!(
                "couldn't read {}: {e}",
                path.display()
            ))),
        }
    }

    /// Records an unfinished submit, or with `None` forgets it.
    pub(crate) fn save_attempt(
        &self,
        repo: &str,
        number: u64,
        attempt: Option<&SubmitAttempt>,
    ) -> Result<(), CoreError> {
        let path = self.attempt_path(repo, number)?;
        let json = attempt.map(|attempt| {
            serde_json::to_vec_pretty(attempt).expect("a submit record serializes to JSON")
        });
        write_file(&self.root, &path, json)?;
        self.notify(repo, number);
        Ok(())
    }

    /// Settles a submit GitHub accepted: deletes the confirmed drafts, unless
    /// edited since, then the record. Drafts added meanwhile stay.
    pub(crate) fn finish_attempt(
        &self,
        repo: &str,
        number: u64,
        sent: &[DraftComment],
    ) -> Result<(), CoreError> {
        self.change(repo, number, |drafts| {
            drafts.retain(|draft| !sent.contains(draft));
            Ok(())
        })?;
        self.save_attempt(repo, number, None)
    }

    /// Next to the drafts file, so it's moved or deleted with it.
    pub(crate) fn attempt_path(&self, repo: &str, number: u64) -> Result<PathBuf, CoreError> {
        Ok(self.path(repo, number)?.with_extension("submit.json"))
    }

    fn path(&self, repo: &str, number: u64) -> Result<PathBuf, CoreError> {
        let (owner, name) = split_repo(repo).ok_or_else(|| {
            CoreError::Storage(format!("invalid repo {repo:?}, expected \"owner/name\""))
        })?;
        // GitHub names are case-insensitive, so "Cli/CLI" and "cli/cli" share one file.
        Ok(self
            .root
            .join("drafts")
            .join(owner.to_ascii_lowercase())
            .join(name.to_ascii_lowercase())
            .join(format!("{number}.json")))
    }

    /// Applies `edit` to a PR's drafts, saves them, then notifies the listener.
    fn change(
        &self,
        repo: &str,
        number: u64,
        edit: impl FnOnce(&mut Vec<DraftComment>) -> Result<(), CoreError>,
    ) -> Result<(), CoreError> {
        let path = self.path(repo, number)?;
        {
            let mut cache = lock(&self.cache);
            let drafts = cached(&mut cache, &path)?;
            // Edit a copy so a failed save leaves the cache matching the disk.
            let mut edited = drafts.clone();
            edit(&mut edited)?;
            save(&self.root, &path, &edited)?;
            *drafts = edited;
        }
        // Outside the cache lock: the listener may call `list` right away.
        self.notify(repo, number);
        Ok(())
    }

    fn notify(&self, repo: &str, number: u64) {
        let listener = lock(&self.listener).clone();
        if let Some(listener) = listener {
            listener.drafts_changed(repo.to_owned(), number);
        }
    }
}

/// Every edit is copy-then-commit, so the data is consistent even after a panic.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn cached<'a>(
    cache: &'a mut HashMap<PathBuf, Vec<DraftComment>>,
    path: &Path,
) -> Result<&'a mut Vec<DraftComment>, CoreError> {
    Ok(match cache.entry(path.to_owned()) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => entry.insert(load(path)?),
    })
}

fn load(path: &Path) -> Result<Vec<DraftComment>, CoreError> {
    match fs::read(path) {
        // An unreadable file is left untouched: it's the user's work.
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            CoreError::Storage(format!(
                "drafts file {} is corrupt ({e}). Fix it or move it aside to keep drafting on this PR.",
                path.display()
            ))
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(CoreError::Storage(format!("couldn't read {}: {e}", path.display()))),
    }
}

/// No drafts means no file.
fn save(root: &Path, path: &Path, drafts: &[DraftComment]) -> Result<(), CoreError> {
    let json = (!drafts.is_empty())
        .then(|| serde_json::to_vec_pretty(drafts).expect("drafts serialize to JSON"));
    write_file(root, path, json)
}

/// Writes a temp file and renames it over the old one, so a crash mid-write
/// can't leave a truncated file. `None` deletes the file. Either way the change
/// is on disk when this returns, even through a power loss. `path` is under `root`.
fn write_file(root: &Path, path: &Path, json: Option<Vec<u8>>) -> Result<(), CoreError> {
    let storage =
        |e: io::Error| CoreError::Storage(format!("couldn't save {}: {e}", path.display()));
    let dir = path.parent().expect("draft paths have a parent");
    match json {
        Some(json) => {
            let tmp = path.with_extension("json.tmp");
            create_dirs(root, dir).map_err(storage)?;
            let mut file = fs::File::create(&tmp).map_err(storage)?;
            file.write_all(&json)
                .and_then(|()| file.sync_all())
                .map_err(storage)?;
            fs::rename(&tmp, path).map_err(storage)?;
        }
        None => match fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            result => result.map_err(storage)?,
        },
    }
    // The rename or unlink is an entry in the directory, so sync that too.
    fs::File::open(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(storage)
}

/// Makes `dir` and any missing parents, then syncs the parent of each level from
/// `root` down, made now or before, so the path to the file survives a power
/// loss, even if a sync failed on an earlier try.
fn create_dirs(root: &Path, dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    for level in dir.ancestors().take_while(|level| level.starts_with(root)) {
        let parent = level
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        fs::File::open(parent.unwrap_or(Path::new(".")))?.sync_all()?;
    }
    Ok(())
}

fn position(drafts: &[DraftComment], id: &str) -> Result<usize, CoreError> {
    drafts
        .iter()
        .position(|draft| draft.id == id)
        .ok_or_else(|| {
            CoreError::Storage(format!("draft {id} not found; it may have been removed"))
        })
}

/// Unique within the process via the counter, and across restarts via the clock.
fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}-{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_failed_checks_and_finds_your_last_review() {
        let commits: OptionalNodes<ChecksCommitNode> = serde_json::from_value(serde_json::json!({ "nodes": [{ "commit": {
            "statusCheckRollup": { "contexts": { "nodes": [
                { "name": "test", "conclusion": "FAILURE", "detailsUrl": "https://ci.example/1" },
                { "name": "lint", "conclusion": "SUCCESS", "detailsUrl": "https://ci.example/2" },
                { "context": "deploy/preview", "state": "ERROR", "targetUrl": null },
                null,
            ] } }
        } }] }))
        .unwrap();
        assert_eq!(
            failed_checks(Some(commits)),
            vec![
                FailedCheck { name: "test".into(), url: Some("https://ci.example/1".into()) },
                FailedCheck { name: "deploy/preview".into(), url: None },
            ]
        );
        assert_eq!(failed_checks(None), vec![]);

        let reviews: OptionalNodes<MyReviewNode> = serde_json::from_value(serde_json::json!({ "nodes": [
            { "viewerDidAuthor": true, "commit": { "oid": "old" } },
            { "viewerDidAuthor": true, "commit": { "oid": "mine" } },
            { "viewerDidAuthor": false, "commit": { "oid": "theirs" } },
        ] }))
        .unwrap();
        assert_eq!(last_reviewed_oid(Some(reviews)), Some("mine".into()));
        assert_eq!(last_reviewed_oid(None), None);
    }
    use std::time::Duration;

    /// Shaped like a real first page: three files (more pending), an outdated
    /// thread, a multi-line thread, a comment from a deleted account, and the
    /// viewer's pending review (behind one that isn't theirs, which GitHub
    /// shouldn't return but is filtered anyway).
    const PR_RESPONSE: &str = r#"{
      "data": { "repository": { "pullRequest": {
        "id": "PR_kwDOAbc123", "title": "Add review window", "body": "Closes #4.",
        "url": "https://github.com/o/r/pull/42", "isDraft": false, "viewerDidAuthor": false,
        "baseRefName": "main", "headRefName": "feat/review",
        "headRefOid": "6dcb09b5b57875f334f61aebed695e2e4193db5e",
        "author": null,
        "reviews": { "nodes": [
          { "id": "PRR_other", "viewerDidAuthor": false, "commit": { "oid": "aaaaaaa" },
            "comments": { "nodes": [
              { "path": "x", "line": 1, "startLine": null, "body": "not mine", "diffHunk": "@@ -1 +1 @@\n+x" }
            ] } },
          { "id": "PRR_mine", "viewerDidAuthor": true,
            "commit": { "oid": "6dcb09b5b57875f334f61aebed695e2e4193db5e" },
            "comments": { "nodes": [
              { "path": "src/pr.rs", "line": 12, "startLine": 8, "originalLine": 12, "originalStartLine": 8,
                "body": "Split this up?",
                "diffHunk": "@@ -1,3 +1,12 @@\n fn a() {}\n+fn b() {}" },
              { "path": "src/lib.rs", "line": 4, "startLine": null, "originalLine": 4, "body": "Why remove this?",
                "diffHunk": "@@ -1,5 +1,4 @@\n use std::io;\n-use std::fs;" },
              { "path": "README.md", "line": null, "startLine": null, "body": "Whole file.", "diffHunk": "" }
            ] } }
        ] },
        "files": {
          "pageInfo": { "hasNextPage": true, "endCursor": "Y3Vyc29yOjM=" },
          "nodes": [
            { "path": "src/lib.rs", "viewerViewedState": "VIEWED" },
            { "path": "src/pr.rs", "viewerViewedState": "UNVIEWED" },
            { "path": "README.md", "viewerViewedState": "DISMISSED" }
          ]
        },
        "reviewThreads": {
          "pageInfo": { "hasNextPage": false, "endCursor": "Y3Vyc29yOjI=" },
          "nodes": [
            {
              "id": "PRRT_outdated", "path": "src/lib.rs", "line": null, "startLine": null,
              "diffSide": "RIGHT", "startDiffSide": null, "isResolved": true, "isOutdated": true,
              "comments": { "nodes": [
                { "author": { "login": "octocat" }, "body": "Typo here.", "state": "SUBMITTED",
                  "createdAt": "2026-11-04T20:57:47Z", "url": "https://github.com/o/r/pull/42#discussion_r1" }
              ] }
            },
            {
              "id": "PRRT_multi", "path": "src/pr.rs", "line": 12, "startLine": 8,
              "diffSide": "RIGHT", "startDiffSide": "LEFT", "isResolved": false, "isOutdated": false,
              "comments": { "nodes": [
                { "author": null, "body": "Can this be one function?", "state": "SUBMITTED",
                  "createdAt": "2026-11-05T09:00:00Z", "url": "https://github.com/o/r/pull/42#discussion_r2",
                  "commit": { "oid": "6dcb09b5b57875f334f61aebed695e2e4193db5e" } },
                { "author": { "login": "hubot" }, "body": "Done.", "state": "SUBMITTED",
                  "createdAt": "2026-11-05T10:30:00Z", "url": "https://github.com/o/r/pull/42#discussion_r3" },
                { "author": { "login": "me" }, "body": "Split this up?", "state": "PENDING",
                  "createdAt": "2026-11-06T08:00:00Z", "url": "https://github.com/o/r/pull/42#discussion_r4" }
              ] }
            },
            {
              "id": "PRRT_pending", "path": "src/lib.rs", "line": 4, "startLine": 4,
              "diffSide": "LEFT", "startDiffSide": "LEFT", "isResolved": false, "isOutdated": false,
              "comments": { "nodes": [
                { "author": { "login": "me" }, "body": "Why remove this?", "state": "PENDING",
                  "createdAt": "2026-11-06T08:01:00Z", "url": "https://github.com/o/r/pull/42#discussion_r5" }
              ] }
            }
          ]
        }
      } } }
    }"#;

    #[test]
    fn parses_pr_detail() {
        let page = parse_pr_page(PR_RESPONSE, "o/r", 42).unwrap();
        let detail = page.detail;
        assert_eq!(detail.id, "PR_kwDOAbc123");
        assert_eq!((detail.repo.as_str(), detail.number), ("o/r", 42));
        assert_eq!(detail.title, "Add review window");
        assert_eq!(detail.body, "Closes #4.");
        assert_eq!(detail.author, None);
        assert!(!detail.is_draft);
        assert_eq!(
            (detail.base_ref_name.as_str(), detail.head_ref_name.as_str()),
            ("main", "feat/review")
        );
        assert_eq!(detail.head_oid, "6dcb09b5b57875f334f61aebed695e2e4193db5e");

        let viewed: Vec<_> = detail
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.viewed))
            .collect();
        assert_eq!(
            viewed,
            [
                ("src/lib.rs", ViewedState::Viewed),
                ("src/pr.rs", ViewedState::Unviewed),
                ("README.md", ViewedState::Dismissed),
            ]
        );
        assert_eq!(page.files_after.as_deref(), Some("Y3Vyc29yOjM="));
        // hasNextPage false: the cursor is ignored.
        assert_eq!(page.threads_after, None);
    }

    fn pending(
        path: &str,
        line: Option<u32>,
        start_line: Option<u32>,
        side: Option<DiffSide>,
        body: &str,
    ) -> PendingComment {
        PendingComment {
            path: path.into(),
            line,
            start_line,
            original_line: line,
            original_start_line: start_line,
            side,
            body: body.into(),
        }
    }

    #[test]
    fn parses_the_viewers_pending_review() {
        let review = parse_pr_page(PR_RESPONSE, "o/r", 42)
            .unwrap()
            .detail
            .pending_review
            .unwrap();
        assert_eq!(review.id, "PRR_mine");
        assert_eq!(
            review.commit_oid,
            "6dcb09b5b57875f334f61aebed695e2e4193db5e"
        );
        assert_eq!(
            review.comments,
            [
                pending(
                    "src/pr.rs",
                    Some(12),
                    Some(8),
                    Some(DiffSide::Right),
                    "Split this up?"
                ),
                pending(
                    "src/lib.rs",
                    Some(4),
                    None,
                    Some(DiffSide::Left),
                    "Why remove this?"
                ),
                pending("README.md", None, None, None, "Whole file."),
            ]
        );
    }

    #[test]
    fn looks_up_the_pending_review_by_node_id() {
        let found = r#"{ "data": { "node": { "reviews": { "nodes": [
          { "id": "PRR_1", "viewerDidAuthor": true, "commit": null, "comments": { "nodes": [] } }
        ] } } } }"#;
        let review = parse_pending_lookup(found).unwrap().unwrap();
        assert_eq!(
            (review.id.as_str(), review.commit_oid.as_str()),
            ("PRR_1", "")
        );

        let none = r#"{ "data": { "node": { "reviews": { "nodes": [] } } } }"#;
        assert_eq!(parse_pending_lookup(none).unwrap(), None);

        let missing = r#"{ "data": { "node": null } }"#;
        assert!(
            parse_pending_lookup(missing)
                .unwrap_err()
                .to_string()
                .contains("wasn't found")
        );

        // About to mutate, so partial data isn't good enough.
        let partial = r#"{ "data": { "node": { "reviews": null } }, "errors": [ { "message": "timeout" } ] }"#;
        assert_eq!(
            parse_pending_lookup(partial).unwrap_err().to_string(),
            "GitHub API error: timeout"
        );
    }

    #[test]
    fn parses_review_threads() {
        let threads = parse_pr_page(PR_RESPONSE, "o/r", 42)
            .unwrap()
            .detail
            .threads;
        // PRRT_pending has only the viewer's pending comment, which `pending_review` shows.
        let ids: Vec<_> = threads.iter().map(|thread| thread.id.as_str()).collect();
        assert_eq!(ids, ["PRRT_outdated", "PRRT_multi"]);

        let outdated = &threads[0];
        assert_eq!(
            (outdated.line, outdated.start_line, outdated.start_side),
            (None, None, None)
        );
        assert!(outdated.is_outdated && outdated.is_resolved);
        assert_eq!(outdated.commit_oid, None);
        assert_eq!(outdated.comments[0].author.as_deref(), Some("octocat"));
        assert_eq!(
            outdated.comments[0].created_at,
            UNIX_EPOCH + Duration::from_secs(1_793_825_867)
        );

        let multi = &threads[1];
        assert_eq!((multi.start_line, multi.line), (Some(8), Some(12)));
        assert_eq!(
            (multi.start_side, multi.side),
            (Some(DiffSide::Left), DiffSide::Right)
        );
        assert!(!multi.is_outdated && !multi.is_resolved);
        assert_eq!(
            multi.commit_oid.as_deref(),
            Some("6dcb09b5b57875f334f61aebed695e2e4193db5e")
        );
        // The viewer's pending reply is left out.
        let authors: Vec<_> = multi.comments.iter().map(|c| c.author.as_deref()).collect();
        assert_eq!(authors, [None, Some("hubot")]);
        assert_eq!(
            multi.comments[1].url,
            "https://github.com/o/r/pull/42#discussion_r3"
        );
    }

    #[test]
    fn later_pages_may_skip_a_connection() {
        let body = r#"{ "data": { "repository": { "pullRequest": {
          "id": "PR_1", "title": "t", "body": "", "url": "u", "isDraft": true, "viewerDidAuthor": true,
          "baseRefName": "main", "headRefName": "h", "headRefOid": "abc", "author": { "login": "me" },
          "reviewThreads": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
        } } } }"#;
        let page = parse_pr_page(body, "o/r", 1).unwrap();
        assert!(page.detail.files.is_empty());
        assert_eq!(page.detail.pending_review, None);
        assert_eq!((page.files_after, page.threads_after), (None, None));
    }

    #[test]
    fn notices_a_push_between_pages() {
        let first = || parse_pr_page(PR_RESPONSE, "o/r", 42).unwrap().detail;
        let mut detail = first();
        let mut later = first();
        assert!(merge_page(&mut detail, later.clone()));
        assert_eq!(detail.threads.len(), 2 * later.threads.len());

        let mut detail = first();
        later.head_oid = "f00dfeed".into();
        assert!(!merge_page(&mut detail, later));
        assert_eq!(detail, first());
    }

    #[test]
    fn reports_a_missing_pr_clearly() {
        let missing_pr = r#"{ "data": { "repository": { "pullRequest": null } },
          "errors": [ { "type": "NOT_FOUND", "message": "Could not resolve to a PullRequest with the number of 9." } ] }"#;
        let missing_repo = r#"{ "data": { "repository": null },
          "errors": [ { "type": "NOT_FOUND", "message": "Could not resolve to a Repository with the name 'o/r'." } ] }"#;
        for body in [missing_pr, missing_repo] {
            assert_eq!(
                parse_pr_page(body, "o/r", 9).unwrap_err().to_string(),
                "GitHub API error: o/r#9 wasn't found, or your gh account can't see it"
            );
        }
        // Any other error says why there's no PR this time.
        let timeout = r#"{ "data": { "repository": { "pullRequest": null } },
          "errors": [ { "message": "Something went wrong while executing your query." } ] }"#;
        assert_eq!(
            parse_pr_page(timeout, "o/r", 9).unwrap_err().to_string(),
            "GitHub API error: Something went wrong while executing your query."
        );
    }

    #[test]
    fn fails_on_errors_without_data_or_with_partial_data() {
        let no_data = r#"{ "errors": [ { "message": "API rate limit exceeded" } ] }"#;
        assert_eq!(
            parse_pr_page(no_data, "o/r", 1).unwrap_err().to_string(),
            "GitHub API error: API rate limit exceeded"
        );
        let partial = PR_RESPONSE.replacen(
            r#""data""#,
            r#""errors": [ { "message": "Something went wrong" } ], "data""#,
            1,
        );
        assert_eq!(
            parse_pr_page(&partial, "o/r", 42).unwrap_err().to_string(),
            "GitHub API error: Something went wrong"
        );
    }

    #[test]
    fn validates_repo_names() {
        assert_eq!(split_repo("cli/cli"), Some(("cli", "cli")));
        assert_eq!(
            split_repo("octo_emu/.github"),
            Some(("octo_emu", ".github"))
        );
        for bad in [
            "cli", "/cli", "cli/", "a/b/c", "../x", "a/..", "a/.", "a b/c", "a/b\0",
        ] {
            assert_eq!(split_repo(bad), None, "{bad:?}");
        }
    }

    fn draft(id: &str, start: Option<(u32, DiffSide)>) -> DraftComment {
        DraftComment {
            id: id.into(),
            path: "src/pr.rs".into(),
            line: 12,
            side: DiffSide::Right,
            start_line: start.map(|(line, _)| line),
            start_side: start.map(|(_, side)| side),
            body: format!("comment {id}"),
            author: DraftAuthor::Agent {
                name: "claude".into(),
            },
            commit_oid: "abc".into(),
        }
    }

    #[test]
    fn starts_a_pending_review_without_an_event() {
        let comments = [draft("1", None), draft("2", Some((8, DiffSide::Left)))];
        let variables = create_pending_variables("PR_1", "abc", &comments);
        assert_eq!(
            variables,
            json!({ "input": {
            "pullRequestId": "PR_1",
            "commitOID": "abc",
            "threads": [
                { "path": "src/pr.rs", "line": 12, "side": "RIGHT", "body": "comment 1" },
                { "path": "src/pr.rs", "line": 12, "side": "RIGHT", "startLine": 8, "startSide": "LEFT",
                  "body": "comment 2" },
            ],
        } })
        );
    }

    #[test]
    fn maps_review_events() {
        let event = |event| submit_pending_variables("PRR_1", event, "")["input"]["event"].clone();
        assert_eq!(event(ReviewEvent::Approve), "APPROVE");
        assert_eq!(event(ReviewEvent::RequestChanges), "REQUEST_CHANGES");
        assert_eq!(event(ReviewEvent::Comment), "COMMENT");
    }

    /// A review an earlier submit recorded, as GitHub has it, and the submit now asked for.
    fn recovery_case(state: &str) -> (SubmitAttempt, RemoteReview, Submission) {
        let sent = draft("1", None);
        let record = SubmitAttempt {
            commit_oid: "abc".into(),
            ..attempt(vec![sent.clone()])
        };
        let remote = RemoteReview {
            state: state.into(),
            viewer_did_author: true,
            url: "https://github.com/o/r/pull/7#pullrequestreview-9".into(),
            body: "LGTM".into(),
            commit_oid: "abc".into(),
            comments: vec![pending(
                "src/pr.rs",
                Some(12),
                None,
                Some(DiffSide::Right),
                "comment 1",
            )],
        };
        let submission = Submission {
            pull_request_id: "PR_1".into(),
            commit_oid: "abc".into(),
            event: ReviewEvent::Approve,
            body: " LGTM\n".into(),
            drafts: vec![sent],
        };
        (record, remote, submission)
    }

    #[test]
    fn a_posted_review_removes_only_the_drafts_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let sent = store.add("o/r".into(), 7, new_draft("sent")).unwrap();
        let gone = store
            .add("o/r".into(), 7, new_draft("deleted on github.com"))
            .unwrap();
        let recorded = [sent.clone(), gone.clone()];
        store
            .save_attempt("o/r", 7, Some(&attempt(recorded.to_vec())))
            .unwrap();

        let warning = finish(&store, "o/r", 7, &recorded, std::slice::from_ref(&sent)).unwrap();
        assert!(
            warning.contains("without 1 of its drafts") && warning.contains("src/pr.rs:3"),
            "{warning}"
        );
        assert_eq!(store.list("o/r".into(), 7).unwrap(), [gone]);
        assert_eq!(store.attempt("o/r", 7).unwrap(), None);
        assert_eq!(finish(&store, "o/r", 7, &[], &[]), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_cleanup_keeps_the_record_and_says_to_submit_again() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let sent = store.add("o/r".into(), 7, new_draft("sent")).unwrap();
        store
            .save_attempt("o/r", 7, Some(&attempt(vec![sent.clone()])))
            .unwrap();
        let drafts_dir = draft_file(&dir).parent().unwrap().to_owned();
        let mode = |mode| fs::set_permissions(&drafts_dir, fs::Permissions::from_mode(mode));
        mode(0o555).unwrap();
        let warning = finish(
            &store,
            "o/r",
            7,
            std::slice::from_ref(&sent),
            std::slice::from_ref(&sent),
        );
        mode(0o755).unwrap();

        let warning = warning.unwrap();
        assert!(warning.contains("Submit again to finish"), "{warning}");
        assert!(!warning.contains("GitHub API error"), "{warning}");
        assert!(store.attempt("o/r", 7).unwrap().is_some());
        assert_eq!(store.list("o/r".into(), 7).unwrap(), [sent]);
    }

    #[test]
    fn tells_a_refusal_from_an_unknown_failure() {
        let ok = r#"{ "data": { "submitPullRequestReview": { "pullRequestReview": { "url": "u#r" } } } }"#;
        assert_eq!(submitted_url(Ok(ok.into())).unwrap(), "u#r");

        let refused = r#"{ "data": { "submitPullRequestReview": null }, "errors": [ { "type": "UNPROCESSABLE",
            "path": ["submitPullRequestReview"], "message": "Can not approve your own pull request" } ] }"#;
        let message = submitted_url(Ok(refused.into())).unwrap_err().to_string();
        assert_eq!(message.matches("GitHub API error").count(), 1, "{message}");
        assert!(
            message.contains("Can not approve your own pull request")
                && message.contains("Nothing was posted")
                && !message.contains("Retrying"),
            "{message}"
        );

        // A timeout has no type: GitHub may have posted it.
        let timeout = r#"{ "data": null, "errors": [ { "message": "Something went wrong while executing your query." } ] }"#;
        let lost = Err(CoreError::Api("HTTP 502 Bad Gateway".into()));
        for answer in [Ok(timeout.into()), lost] {
            let message = submitted_url(answer).unwrap_err().to_string();
            assert_eq!(message.matches("GitHub API error").count(), 1, "{message}");
            assert!(message.contains("Retrying is safe"), "{message}");
        }
    }

    #[test]
    fn a_review_whose_commit_github_left_out_isnt_settled() {
        let (_, review, _) = recovery_case("COMMENTED");
        assert_eq!(known_commit(None).unwrap(), None);
        assert_eq!(
            known_commit(Some(review.clone())).unwrap(),
            Some(review.clone())
        );
        let unknown = RemoteReview {
            commit_oid: String::new(),
            ..review
        };
        let message = known_commit(Some(unknown)).unwrap_err().to_string();
        assert!(message.contains("won't post the review twice"), "{message}");
    }

    #[test]
    fn settles_an_earlier_submit_by_what_github_has() {
        let (record, remote, submission) = recovery_case("APPROVED");
        assert_eq!(
            recover(&record, Some(&remote), &submission),
            Recovery::Posted
        );
        // Deleted while pending.
        assert_eq!(recover(&record, None, &submission), Recovery::Forget);

        let (record, mut remote, submission) = recovery_case("PENDING");
        assert_eq!(
            recover(&record, Some(&remote), &submission),
            Recovery::Resume
        );
        remote.commit_oid = "old".into();
        assert_eq!(
            recover(&record, Some(&remote), &submission),
            Recovery::Elsewhere
        );
    }

    #[test]
    fn a_different_posted_review_removes_only_the_drafts_it_has() {
        let (mut record, mut remote, mut submission) = recovery_case("COMMENTED");
        let mut other = draft("2", None);
        other.line = 30;
        record.drafts.push(other.clone());
        submission.drafts.push(other.clone());
        let mut late = draft("3", None);
        late.line = 40;
        submission.drafts.push(late.clone());
        // Draft 2 was deleted from the review on github.com before it was submitted.
        remote.comments.retain(|comment| comment.line == Some(12));
        assert_eq!(
            recover(&record, Some(&remote), &submission),
            Recovery::PostedDifferently
        );

        let sent = present(&record.drafts, &remote.comments);
        assert_eq!(sent, [record.drafts[0].clone()]);
        assert_eq!(
            stayed(&submission.drafts, &sent),
            "Drafts not in it are still here: src/pr.rs:30, src/pr.rs:40."
        );
        assert_eq!(
            stayed(&record.drafts[..1], &sent),
            "Its drafts were all in it, so they're removed here."
        );
    }

    #[test]
    fn reports_success_only_for_exactly_what_was_asked() {
        let differently = |change: &dyn Fn(&mut RemoteReview, &mut Submission)| {
            let (record, mut remote, mut submission) = recovery_case("APPROVED");
            change(&mut remote, &mut submission);
            recover(&record, Some(&remote), &submission)
        };
        // The approval was for an older commit, and this one is on a newer push.
        assert_eq!(
            differently(&|_, s| s.commit_oid = "new".into()),
            Recovery::PostedDifferently
        );
        assert_eq!(
            differently(&|_, s| s.event = ReviewEvent::RequestChanges),
            Recovery::PostedDifferently
        );
        // Submitted on github.com with another verdict or summary.
        assert_eq!(
            differently(&|r, _| r.state = "COMMENTED".into()),
            Recovery::PostedDifferently
        );
        assert_eq!(
            differently(&|r, _| r.body = "Ship it".into()),
            Recovery::PostedDifferently
        );
        assert_eq!(
            differently(&|r, _| r.state = "DISMISSED".into()),
            Recovery::PostedDifferently
        );
        // A draft added since, and one deleted from the review on github.com.
        assert_eq!(
            differently(&|_, s| s.drafts.push(draft("2", None))),
            Recovery::PostedDifferently
        );
        assert_eq!(
            differently(&|r, _| r.comments.clear()),
            Recovery::PostedDifferently
        );
        // A review someone else wrote isn't yours, even with the same verdict and text.
        assert_eq!(
            differently(&|r, _| r.viewer_did_author = false),
            Recovery::PostedDifferently
        );
        let (record, mut remote, _) = recovery_case("APPROVED");
        assert_eq!(sent_in(&record, &remote), record.drafts);
        remote.viewer_did_author = false;
        assert!(sent_in(&record, &remote).is_empty());
        // Extra comments added on github.com don't change what this submit asked for.
        assert_eq!(
            differently(&|r, _| r.comments.push(pending(
                "x",
                Some(1),
                None,
                Some(DiffSide::Right),
                "mine"
            ))),
            Recovery::Posted
        );
    }

    #[test]
    fn reads_a_review_page_and_only_not_found_as_gone() {
        let page = r#"{ "data": { "node": {
          "state": "PENDING", "url": "https://github.com/o/r/pull/7#pullrequestreview-9", "body": "",
          "viewerDidAuthor": true,
          "commit": { "oid": "abc" },
          "comments": { "pageInfo": { "hasNextPage": true, "endCursor": "C1" }, "nodes": [
            { "path": "src/pr.rs", "line": 12, "startLine": null, "body": "comment 1",
              "diffHunk": "@@ -1,2 +1,12 @@\n+fn b() {}" } ] } } } }"#;
        let (review, next) = parse_review_page(page).unwrap().unwrap();
        assert_eq!(
            (review.state.as_str(), review.commit_oid.as_str()),
            ("PENDING", "abc")
        );
        assert_eq!(review.comments[0].body, "comment 1");
        assert_eq!(next.as_deref(), Some("C1"));

        let deleted = r#"{ "data": { "node": null }, "errors": [ { "type": "NOT_FOUND", "path": ["node"],
          "message": "Could not resolve to a node with the global id of 'PRR_1'" } ] }"#;
        assert_eq!(parse_review_page(deleted).unwrap(), None);
        // Anything else may be passing, and the review may still exist.
        let other_type = r#"{ "data": { "node": null }, "errors": [ { "type": "FORBIDDEN", "path": ["node"], "message": "no" } ] }"#;
        let no_type =
            r#"{ "data": { "node": null }, "errors": [ { "message": "Something went wrong" } ] }"#;
        let elsewhere = r#"{ "data": { "node": null }, "errors": [ { "type": "NOT_FOUND", "path": ["node", "commit"], "message": "x" } ] }"#;
        for body in [
            other_type,
            no_type,
            elsewhere,
            r#"{ "data": { "node": null } }"#,
            r#"{ "errors": [ { "message": "API rate limit exceeded" } ] }"#,
        ] {
            assert!(parse_review_page(body).is_err(), "{body}");
        }
    }

    #[test]
    fn lists_the_drafts_already_in_a_pending_review() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let sent = store.add("o/r".into(), 7, new_draft("sent")).unwrap();
        store.add("o/r".into(), 7, new_draft("late")).unwrap();
        assert_eq!(store.unsettled("o/r".into(), 7).unwrap(), None);
        store
            .save_attempt("o/r", 7, Some(&attempt(vec![sent.clone()])))
            .unwrap();
        assert_eq!(
            store.unsettled("o/r".into(), 7).unwrap(),
            Some(UnsettledSubmit {
                review_url: "https://github.com/o/r/pull/7#pullrequestreview-9".into(),
                draft_ids: vec![sent.id],
            })
        );
    }

    #[test]
    fn tells_a_pending_discarded_review_from_one_github_couldnt_check() {
        let url = "https://github.com/o/r/pull/7#pullrequestreview-9";
        let (_, review, _) = recovery_case("PENDING");
        assert_eq!(
            what_is_left(&Ok(Some(review.clone())), url),
            Some(LeftOnGitHub::Pending { url: url.into() })
        );
        assert_eq!(
            what_is_left(&Err(CoreError::Network("offline".into())), url),
            Some(LeftOnGitHub::Unknown { url: url.into() })
        );
        assert_eq!(what_is_left(&Ok(None), url), None);
        let submitted = RemoteReview {
            state: "APPROVED".into(),
            ..review
        };
        assert_eq!(what_is_left(&Ok(Some(submitted)), url), None);
    }

    #[test]
    fn builds_pending_review_variables() {
        assert_eq!(
            add_thread_variables("PRR_1", &draft("2", Some((8, DiffSide::Left)))),
            json!({ "input": {
                "pullRequestReviewId": "PRR_1", "path": "src/pr.rs", "line": 12, "side": "RIGHT",
                "startLine": 8, "startSide": "LEFT", "body": "comment 2",
            } })
        );
        assert_eq!(
            add_thread_variables("PRR_1", &draft("1", None)),
            json!({ "input": {
                "pullRequestReviewId": "PRR_1", "path": "src/pr.rs", "line": 12, "side": "RIGHT",
                "body": "comment 1",
            } })
        );
        assert_eq!(
            submit_pending_variables("PRR_1", ReviewEvent::Approve, "LGTM"),
            json!({ "input": { "pullRequestReviewId": "PRR_1", "event": "APPROVE", "body": "LGTM" } })
        );
    }

    #[test]
    fn skips_drafts_already_in_the_pending_review() {
        let mut single_with_start = draft("single", None);
        single_with_start.start_line = Some(12);
        let mut other_line = draft("moved", None);
        other_line.line = 13;
        let drafts = [
            draft("1", None),
            draft("2", Some((8, DiffSide::Left))),
            draft("edited", None),
            single_with_start,
            other_line,
        ];
        let pending = [
            // A retry after "1" and "2" went in. Bodies compare trimmed; context lines' sides aren't known.
            pending("src/pr.rs", Some(12), None, None, "comment 1\n"),
            pending("src/pr.rs", Some(12), Some(8), None, "comment 2"),
            pending("src/pr.rs", Some(12), None, None, "comment single"),
            pending("src/pr.rs", Some(12), None, None, "comment moved"),
            pending("src/pr.rs", Some(12), None, None, "something else"),
        ];
        let ids: Vec<_> = missing(&drafts, &pending)
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(ids, ["edited", "moved"]);
        assert_eq!(missing(&drafts, &[]).len(), 5);
    }

    #[test]
    fn matches_drafts_to_comments_one_to_one() {
        let twin = |id: &str| DraftComment {
            body: "Same".into(),
            ..draft(id, None)
        };
        let comment = pending("src/pr.rs", Some(12), None, None, "Same");
        // Two identical drafts need two comments.
        let drafts = [twin("a"), twin("b")];
        let one = std::slice::from_ref(&comment);
        assert_eq!(present(&drafts, one), [twin("a")]);
        assert_eq!(missing(&drafts, one), [&twin("b")]);
        assert!(missing(&drafts, &[comment.clone(), comment.clone()]).is_empty());

        // A push moved the comment's current line; its original one still matches.
        let moved = PendingComment {
            line: Some(20),
            ..comment.clone()
        };
        assert_eq!(present(&drafts[..1], &[moved]), [twin("a")]);

        // A comment known to be on a removed line isn't a right-side draft.
        let left = PendingComment {
            side: Some(DiffSide::Left),
            ..comment.clone()
        };
        assert!(present(&drafts[..1], std::slice::from_ref(&left)).is_empty());
        // ...and a left draft takes it before a right comment it could also take.
        let left_draft = DraftComment {
            side: DiffSide::Left,
            ..twin("l")
        };
        let both = [left_draft.clone(), twin("r")];
        assert_eq!(present(&both, &[comment.clone(), left]), both);

        // GitHub left out the original range: the current one stands in.
        let unoriginal = PendingComment {
            original_line: None,
            ..comment
        };
        assert_eq!(present(&drafts[..1], &[unoriginal]), [twin("a")]);
    }

    #[test]
    fn refuses_a_pending_review_on_another_commit() {
        let head = "6dcb09b5b57875f334f61aebed695e2e4193db5e";
        assert!(check_pending_commit(head, head).is_ok());
        let message = check_pending_commit("1234567890abcdef", head)
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("on commit 1234567") && message.contains("reviewing 6dcb09b"),
            "{message}"
        );
    }

    #[test]
    fn adds_drafts_to_a_pending_review_only_while_their_commit_is_the_head() {
        let head = "6dcb09b5b57875f334f61aebed695e2e4193db5e";
        assert!(check_threads_land(head, head, "u#r").is_ok());
        let message = check_threads_land(head, "1234567890abcdef", "u#r")
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("on commit 1234567") && message.ends_with("u#r"),
            "{message}"
        );
    }

    #[test]
    fn an_added_lines_comment_isnt_the_removed_lines_draft() {
        // The same text on removed line 12 and added line 12. The left comment was
        // deleted on github.com, so the review holds only the right one.
        let right = DraftComment {
            body: "Same".into(),
            ..draft("r", None)
        };
        let left = DraftComment {
            id: "l".into(),
            side: DiffSide::Left,
            ..right.clone()
        };
        let added = pending("src/pr.rs", Some(12), None, Some(DiffSide::Right), "Same");
        let drafts = [left.clone(), right.clone()];
        assert_eq!(present(&drafts, std::slice::from_ref(&added)), [right]);
        assert_eq!(missing(&drafts, &[added]), [&left]);

        let side = |hunk: &str| {
            pending_comment(PendingCommentNode {
                path: "src/pr.rs".into(),
                line: Some(12),
                start_line: None,
                original_line: Some(12),
                original_start_line: None,
                body: "Same".into(),
                diff_hunk: hunk.into(),
            })
            .side
        };
        assert_eq!(side("@@ -12 +12 @@\n-old\r\n+new\r"), Some(DiffSide::Right));
        assert_eq!(side("@@ -12 +12 @@\n-old"), Some(DiffSide::Left));
        assert_eq!(side("@@ -12 +12 @@\n same"), None);
        assert_eq!(side(""), None);
    }

    #[test]
    fn shortens_commits_like_github() {
        assert_eq!(
            short_oid("6dcb09b5b57875f334f61aebed695e2e4193db5e"),
            "6dcb09b"
        );
        assert_eq!(short_oid("abc"), "abc");
    }

    #[test]
    fn mutation_returns_data_on_success() {
        let body = r#"{ "data": { "addPullRequestReview": { "pullRequestReview": { "url": "https://github.com/o/r/pull/1#pullrequestreview-9" } } } }"#;
        let data = parse_mutation(body).unwrap();
        assert_eq!(
            data.pointer("/addPullRequestReview/pullRequestReview/url")
                .unwrap(),
            "https://github.com/o/r/pull/1#pullrequestreview-9"
        );
    }

    #[test]
    fn mutation_errors_keep_github_message() {
        let body = r#"{ "data": { "addPullRequestReview": null },
          "errors": [ { "type": "UNPROCESSABLE", "message": "Line could not be resolved" } ] }"#;
        assert_eq!(
            parse_mutation(body).unwrap_err().to_string(),
            "GitHub API error: Line could not be resolved"
        );
    }

    #[test]
    fn explains_an_existing_pending_review() {
        let body = r#"{ "data": { "addPullRequestReview": null },
          "errors": [ { "message": "User can only have one pending review per pull request" } ] }"#;
        let message = parse_mutation(body).unwrap_err().to_string();
        assert!(
            message.contains("you just started a pending review"),
            "{message}"
        );
        assert!(
            message.contains("User can only have one pending review per pull request"),
            "{message}"
        );
    }

    // --- DraftStore ---

    fn new_draft(body: &str) -> NewDraft {
        NewDraft {
            path: "src/pr.rs".into(),
            line: 3,
            side: DiffSide::Right,
            start_line: None,
            start_side: None,
            body: body.into(),
            author: DraftAuthor::You,
            commit_oid: "c0ffee0123456789".into(),
        }
    }

    fn store(dir: &tempfile::TempDir) -> DraftStore {
        DraftStore::new(dir.path().to_str().unwrap().into())
    }

    fn draft_file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("drafts/o/r/7.json")
    }

    #[test]
    fn draft_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        assert!(store.list("o/r".into(), 7).unwrap().is_empty());

        let first = store.add("o/r".into(), 7, new_draft("first")).unwrap();
        let second = store.add("o/r".into(), 7, new_draft("second")).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(
            store.list("o/r".into(), 7).unwrap(),
            [first.clone(), second.clone()]
        );
        assert!(draft_file(&dir).exists());

        store
            .update_body("o/r".into(), 7, first.id.clone(), "edited".into())
            .unwrap();
        store.remove("o/r".into(), 7, second.id.clone()).unwrap();
        let drafts = store.list("o/r".into(), 7).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].body, "edited");

        assert!(store.remove("o/r".into(), 7, second.id).is_err());
        assert!(
            store
                .update_body("o/r".into(), 7, "nope".into(), "x".into())
                .is_err()
        );

        store.clear("o/r".into(), 7).unwrap();
        assert!(store.list("o/r".into(), 7).unwrap().is_empty());
        assert!(!draft_file(&dir).exists());
        // Other PRs are separate.
        assert!(store.list("o/r".into(), 8).unwrap().is_empty());
    }

    #[test]
    fn drafts_persist_across_stores() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = new_draft("from the agent");
        agent.start_line = Some(1);
        agent.start_side = Some(DiffSide::Left);
        agent.author = DraftAuthor::Agent {
            name: "codex".into(),
        };
        let added = store(&dir).add("o/r".into(), 7, agent).unwrap();
        // Repo names are case-insensitive on GitHub.
        assert_eq!(store(&dir).list("O/R".into(), 7).unwrap(), [added]);
        assert_eq!(
            fs::read_dir(draft_file(&dir).parent().unwrap())
                .unwrap()
                .count(),
            1,
            "no temp file left"
        );
    }

    #[test]
    fn pins_drafts_to_one_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        assert_eq!(store.pinned_commit("o/r".into(), 7).unwrap(), None);

        store.add("o/r".into(), 7, new_draft("first")).unwrap();
        assert_eq!(
            store.pinned_commit("o/r".into(), 7).unwrap().as_deref(),
            Some("c0ffee0123456789")
        );

        let mut newer = new_draft("on a newer push");
        newer.commit_oid = "deadbeef0000".into();
        let error = store.add("o/r".into(), 7, newer.clone()).unwrap_err();
        assert!(matches!(error, CoreError::Storage(_)));
        assert!(
            error.to_string().contains("pinned to commit c0ffee0"),
            "{error}"
        );
        assert_eq!(store.list("o/r".into(), 7).unwrap().len(), 1);

        // Once the drafts are gone, any commit goes.
        store.clear("o/r".into(), 7).unwrap();
        assert_eq!(store.pinned_commit("o/r".into(), 7).unwrap(), None);
        store.add("o/r".into(), 7, newer).unwrap();
        assert_eq!(
            store.pinned_commit("o/r".into(), 7).unwrap().as_deref(),
            Some("deadbeef0000")
        );
    }

    struct Recorder {
        store: Arc<DraftStore>,
        calls: Mutex<Vec<(String, u64, usize)>>,
    }

    impl DraftListener for Recorder {
        fn drafts_changed(&self, repo: String, number: u64) {
            // Reading back from the callback must not deadlock.
            let count = self.store.list(repo.clone(), number).unwrap().len();
            self.calls.lock().unwrap().push((repo, number, count));
        }
    }

    #[test]
    fn notifies_listener_after_each_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(store(&dir));
        let recorder = Arc::new(Recorder {
            store: store.clone(),
            calls: Mutex::default(),
        });
        store.set_listener(recorder.clone());

        let draft = store.add("o/r".into(), 7, new_draft("hi")).unwrap();
        store
            .update_body("o/r".into(), 7, draft.id.clone(), "hello".into())
            .unwrap();
        assert!(store.remove("o/r".into(), 7, "missing".into()).is_err());
        store.clear("o/r".into(), 7).unwrap();
        assert_eq!(
            *recorder.calls.lock().unwrap(),
            [
                ("o/r".to_string(), 7, 1),
                ("o/r".to_string(), 7, 1),
                ("o/r".to_string(), 7, 0),
            ]
        );
    }

    #[test]
    fn leaves_a_corrupt_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = draft_file(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[{ not json").unwrap();

        let store = store(&dir);
        assert!(matches!(
            store.list("o/r".into(), 7),
            Err(CoreError::Storage(_))
        ));
        assert!(matches!(
            store.add("o/r".into(), 7, new_draft("x")),
            Err(CoreError::Storage(_))
        ));
        assert!(matches!(
            store.clear("o/r".into(), 7),
            Err(CoreError::Storage(_))
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "[{ not json");
    }

    fn attempt(drafts: Vec<DraftComment>) -> SubmitAttempt {
        SubmitAttempt {
            pr_node_id: "PR_1".into(),
            viewer_id: "U_1".into(),
            viewer_login: "octocat".into(),
            review_id: "PRR_1".into(),
            review_url: "https://github.com/o/r/pull/7#pullrequestreview-9".into(),
            commit_oid: "c0ffee0123456789".into(),
            event: ReviewEvent::Approve,
            body: "LGTM".into(),
            drafts,
        }
    }

    #[test]
    fn records_a_submit_without_drafts() {
        let dir = tempfile::tempdir().unwrap();
        store(&dir)
            .save_attempt("o/r", 7, Some(&attempt(vec![])))
            .unwrap();
        // Survives a restart, so a retry after a lost answer still finds the review.
        let reopened = store(&dir);
        assert_eq!(reopened.attempt("O/R", 7).unwrap(), Some(attempt(vec![])));
        reopened.finish_attempt("o/r", 7, &[]).unwrap();
        assert_eq!(reopened.attempt("o/r", 7).unwrap(), None);
    }

    #[test]
    fn finishing_a_submit_removes_only_the_drafts_it_sent() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let sent = store.add("o/r".into(), 7, new_draft("sent")).unwrap();
        store
            .save_attempt("o/r", 7, Some(&attempt(vec![sent])))
            .unwrap();
        // An agent drafts while GitHub is still answering.
        let late = store.add("o/r".into(), 7, new_draft("late")).unwrap();
        store
            .finish_attempt("o/r", 7, &store.attempt("o/r", 7).unwrap().unwrap().drafts)
            .unwrap();
        assert_eq!(store.list("o/r".into(), 7).unwrap(), [late]);
        assert_eq!(store.attempt("o/r", 7).unwrap(), None);
    }

    #[test]
    fn submits_only_the_confirmed_drafts_as_they_were() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mine = store.add("o/r".into(), 7, new_draft("mine")).unwrap();
        let ids = [mine.id.clone()];
        let confirmed = store.confirmed("o/r", 7, &ids).unwrap();
        // An agent drafts after the reviewer clicked Submit.
        let late = store.add("o/r".into(), 7, new_draft("late")).unwrap();
        assert_eq!(store.confirmed("o/r", 7, &ids).unwrap(), confirmed);
        assert!(store.confirmed("o/r", 7, &["gone".into()]).is_err());

        // Only a draft still as it was sent is deleted: matched by id and content.
        store
            .save_attempt("o/r", 7, Some(&attempt(confirmed.clone())))
            .unwrap();
        store
            .update_body("o/r".into(), 7, mine.id.clone(), "edited".into())
            .unwrap();
        store.finish_attempt("o/r", 7, &confirmed).unwrap();
        let left: Vec<_> = store
            .list("o/r".into(), 7)
            .unwrap()
            .into_iter()
            .map(|d| d.body)
            .collect();
        assert_eq!(left, ["edited", late.body.as_str()]);
    }

    #[test]
    fn discarding_drafts_forgets_an_unfinished_submit() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let draft = store.add("o/r".into(), 7, new_draft("x")).unwrap();
        store
            .save_attempt("o/r", 7, Some(&attempt(vec![draft])))
            .unwrap();
        store.clear("o/r".into(), 7).unwrap();
        assert_eq!(store.attempt("o/r", 7).unwrap(), None);
        assert!(!draft_file(&dir).exists());
    }

    #[test]
    fn makes_missing_directories_one_by_one() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("drafts/owner/name");
        create_dirs(dir.path(), &deep).unwrap();
        assert!(deep.is_dir());
        // Already there is fine, and a file in the way isn't.
        create_dirs(dir.path(), &deep).unwrap();
        fs::write(dir.path().join("file"), "x").unwrap();
        assert!(create_dirs(dir.path(), &dir.path().join("file/below")).is_err());
    }

    #[test]
    fn rejects_repos_that_could_escape_the_drafts_dir() {
        let dir = tempfile::tempdir().unwrap();
        let error = store(&dir)
            .add("../../etc".into(), 1, new_draft("x"))
            .unwrap_err();
        assert!(matches!(error, CoreError::Storage(_)));
    }

    /// Hits the real API with the local `gh` login: `cargo test -- --ignored`.
    /// Read-only: never add a test here that runs a mutation.
    #[tokio::test]
    #[ignore = "needs network and gh login"]
    async fn fetches_live_pull_request() {
        // Merged, with 252 files (three pages) and review threads.
        let detail = GitHubClient::new()
            .fetch_pull_request("cli/cli".into(), 14278)
            .await
            .unwrap();
        assert_eq!(detail.files.len(), 252);
        assert!(detail.threads.len() >= 10);
        assert!(
            detail
                .threads
                .iter()
                .all(|thread| !thread.comments.is_empty())
        );
        assert_eq!(detail.head_oid.len(), 40);
        assert!(detail.id.starts_with("PR_"));
        // Nobody else's pending review is visible, and the viewer has none here.
        assert_eq!(detail.pending_review, None);
    }
}

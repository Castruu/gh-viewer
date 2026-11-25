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
      id title body url isDraft baseRefName headRefName headRefOid
      author { login }
      ...pendingReviews
      # GitHub caps this connection at 3000 files.
      files(first: 100, after: $filesAfter) @include(if: $withFiles) {
        pageInfo { hasNextPage endCursor }
        nodes { path viewerViewedState }
      }
      reviewThreads(first: 100, after: $threadsAfter) @include(if: $withThreads) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id path line startLine diffSide startDiffSide isResolved isOutdated
          comments(first: 100) { nodes { author { login } body createdAt url state } }
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
      comments(first: 100) { nodes { path line startLine body diffHunk } }
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
  addPullRequestReview(input: $input) { pullRequestReview { id } }
}
"#;

/// The review an earlier submit started: still pending with its comments, or submitted.
const RECORDED_REVIEW: &str = r#"
query($id: ID!) {
  node(id: $id) {
    ... on PullRequestReview {
      id state url viewerDidAuthor commit { oid }
      comments(first: 100) { nodes { path line startLine body diffHunk } }
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
    pub is_draft: bool,
    pub base_ref_name: String,
    pub head_ref_name: String,
    /// GitHub's head commit when this was fetched. Thread lines refer to it.
    pub head_oid: String,
    pub files: Vec<ChangedFile>,
    pub threads: Vec<ReviewThread>,
    /// A review you started on github.com and haven't submitted. Submitting
    /// adds your drafts to it and submits it.
    pub pending_review: Option<PendingReview>,
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
    /// Inferred from the diff hunk, as GitHub doesn't report it for review
    /// comments: `Left` only when the commented line was removed.
    pub side: DiffSide,
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
        while files_after.is_some() || threads_after.is_some() {
            variables["withFiles"] = files_after.is_some().into();
            variables["filesAfter"] = files_after.into();
            variables["withThreads"] = threads_after.is_some().into();
            variables["threadsAfter"] = threads_after.into();
            let page = parse_pr_page(
                &self.graphql(&query, variables.clone()).await?,
                &repo,
                number,
            )?;
            detail.files.extend(page.detail.files);
            detail.threads.extend(page.detail.threads);
            (files_after, threads_after) = (page.files_after, page.threads_after);
        }
        Ok(detail)
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

/// What the reviewer confirmed in the submit sheet.
pub(crate) struct Submission {
    pub pull_request_id: String,
    pub commit_oid: String,
    pub event: ReviewEvent,
    pub body: String,
    pub drafts: Vec<DraftComment>,
}

/// A submit whose drafts are in a pending review on GitHub, saved next to the
/// drafts before the review is submitted. A retry settles it first, so a lost
/// answer never leads to a second review, with or without drafts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SubmitAttempt {
    pub review_id: String,
    pub commit_oid: String,
    pub event: ReviewEvent,
    pub body: String,
    /// The drafts in the review, as they were sent. One edited since is a different draft.
    pub drafts: Vec<DraftComment>,
}

impl GitHubClient {
    /// Submits a review in two steps: the drafts go into a pending review, which
    /// is recorded next to them, then submitted. Returns the review's URL.
    /// Retrying after any failure is safe: it never posts the review twice.
    pub(crate) async fn submit_review(
        &self,
        store: &DraftStore,
        repo: &str,
        number: u64,
        submission: Submission,
    ) -> Result<String, CoreError> {
        let attempt = match store.attempt(repo, number)? {
            // Settled before anything else, even another pending review of yours.
            Some(attempt) => {
                let recorded = self.recorded_review(&attempt.review_id).await.map_err(|error| {
                    CoreError::Api(format!(
                        "couldn't check the review an earlier submit started ({error}). Its record \
                         is kept, so retrying can't post twice. If you deleted that review on \
                         github.com, move {} aside to submit again.",
                        store.attempt_path(repo, number).unwrap_or_default().display()
                    ))
                })?;
                if recorded.state != "PENDING" {
                    // An earlier try went through; only its answer was lost.
                    store
                        .finish_attempt(repo, number, &attempt)
                        .map_err(|error| posted_but(&recorded.url, error))?;
                    let unsent = unsent(&attempt, &submission);
                    if unsent.is_empty() {
                        return Ok(recorded.url);
                    }
                    return Err(CoreError::Api(format!(
                        "an earlier try already posted your review as {} ({}), so this one wasn't \
                         sent. Not in it: {}. Submit again to send that as a new review.",
                        posted_as(&recorded.state),
                        recorded.url,
                        unsent.join("; ")
                    )));
                }
                self.resume(attempt, recorded.review, &submission).await?
            }
            None => self.start(&submission).await?,
        };
        // Recorded before the last step: if its answer is lost, a retry finds the review submitted.
        store.save_attempt(repo, number, Some(&attempt))?;
        let url = self
            .submit_pending_review(&attempt.review_id, attempt.event, &attempt.body)
            .await?;
        store
            .finish_attempt(repo, number, &attempt)
            .map_err(|error| posted_but(&url, error))?;
        Ok(url)
    }

    /// Puts the drafts in the viewer's pending review on the submission's
    /// commit: one from github.com, whose comments then go out too, or a new one.
    async fn start(&self, submission: &Submission) -> Result<SubmitAttempt, CoreError> {
        let lookup = format!("{PENDING_LOOKUP}{PENDING_REVIEWS}");
        let pending = parse_pending_lookup(
            &self
                .graphql(&lookup, json!({ "id": submission.pull_request_id }))
                .await?,
        )?;
        let review_id = match pending {
            Some(pending) => {
                check_pending_commit(&pending, &submission.commit_oid)?;
                let new = new_drafts(&submission.drafts, &pending.comments);
                self.add_threads(&pending.id, &new).await?;
                pending.id
            }
            None => {
                let variables = create_pending_variables(
                    &submission.pull_request_id,
                    &submission.commit_oid,
                    &submission.drafts,
                );
                let data = parse_mutation(&self.graphql(CREATE_PENDING, variables).await?)?;
                data.pointer("/addPullRequestReview/pullRequestReview/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| CoreError::Api("GitHub's response had no review id".into()))?
            }
        };
        Ok(SubmitAttempt {
            review_id,
            commit_oid: submission.commit_oid.clone(),
            event: submission.event,
            body: submission.body.clone(),
            drafts: submission.drafts.clone(),
        })
    }

    /// Continues an earlier attempt in its own pending review, and only that one.
    async fn resume(
        &self,
        attempt: SubmitAttempt,
        review: PendingReview,
        submission: &Submission,
    ) -> Result<SubmitAttempt, CoreError> {
        check_pending_commit(&review, &submission.commit_oid)?;
        let new = new_drafts(&submission.drafts, &review.comments);
        self.add_threads(&review.id, &new).await?;
        let mut drafts = attempt.drafts;
        for draft in &submission.drafts {
            if !drafts.contains(draft) {
                drafts.push(draft.clone());
            }
        }
        Ok(SubmitAttempt {
            review_id: review.id,
            commit_oid: submission.commit_oid.clone(),
            event: submission.event,
            body: submission.body.clone(),
            drafts,
        })
    }

    async fn add_threads(&self, review_id: &str, new: &[&DraftComment]) -> Result<(), CoreError> {
        // Retrying after a failure is safe: drafts already in the pending review are skipped.
        let partial = |added: usize, error: CoreError| {
            CoreError::Api(format!(
                "added {added} of {} comments to your pending review on GitHub, then failed: {error}. \
                 Retrying is safe: comments already added are skipped.",
                new.len()
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

    /// The review an earlier submit recorded. Any error fails, even with a null
    /// node: it may be passing, and a review that still exists must not be redone.
    async fn recorded_review(&self, review_id: &str) -> Result<RecordedReview, CoreError> {
        parse_recorded_review(
            &self
                .graphql(RECORDED_REVIEW, json!({ "id": review_id }))
                .await?,
        )
    }

    /// Phase 2 of a submit: submits the pending review; returns its URL.
    async fn submit_pending_review(
        &self,
        review_id: &str,
        event: ReviewEvent,
        body: &str,
    ) -> Result<String, CoreError> {
        self.graphql(SUBMIT_PENDING, submit_pending_variables(review_id, event, body))
            .await
            .and_then(|response| parse_mutation(&response))
            .and_then(|data| review_url(&data, "/submitPullRequestReview/pullRequestReview/url"))
            .map_err(|error| {
                CoreError::Api(format!(
                    "your comments are in a pending review on GitHub, but submitting it failed: {error}. \
                     Retrying is safe: it won't post the review twice."
                ))
            })
    }
}

/// What this submission has that an earlier attempt, already posted, didn't send.
fn unsent(attempt: &SubmitAttempt, submission: &Submission) -> Vec<String> {
    let mut unsent = Vec::new();
    if submission.event != attempt.event {
        let verdict = match submission.event {
            ReviewEvent::Approve => "Approve",
            ReviewEvent::RequestChanges => "Request Changes",
            ReviewEvent::Comment => "Comment",
        };
        unsent.push(format!("the {verdict} verdict"));
    }
    if submission.body.trim() != attempt.body.trim() {
        unsent.push("this summary".into());
    }
    let drafts: Vec<String> = submission
        .drafts
        .iter()
        .filter(|draft| !attempt.drafts.contains(draft))
        .map(|draft| format!("{}:{}", draft.path, draft.line))
        .collect();
    if !drafts.is_empty() {
        unsent.push(format!(
            "the drafts on {}, which are still here",
            drafts.join(", ")
        ));
    }
    unsent
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

fn posted_but(url: &str, error: CoreError) -> CoreError {
    CoreError::Storage(format!(
        "your review was posted ({url}), but its drafts couldn't be deleted ({error}). \
         Submitting again deletes them without posting the review twice."
    ))
}

/// A review an earlier submit started, as GitHub has it now.
#[derive(Debug, PartialEq)]
struct RecordedReview {
    /// PENDING until submitted; then COMMENTED, APPROVED, CHANGES_REQUESTED or DISMISSED.
    state: String,
    url: String,
    review: PendingReview,
}

/// Parses `RECORDED_REVIEW`.
fn parse_recorded_review(body: &str) -> Result<RecordedReview, CoreError> {
    #[derive(Deserialize)]
    struct Node {
        state: String,
        url: String,
        #[serde(flatten)]
        review: ReviewNode,
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
            "GitHub returned no review for its id".into(),
        ));
    }
    let node: Node = serde_json::from_value(node).map_err(unexpected)?;
    Ok(RecordedReview {
        state: node.state,
        url: node.url,
        review: pending_review(node.review),
    })
}

fn review_url(data: &Value, pointer: &str) -> Result<String, CoreError> {
    data.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| CoreError::Api("GitHub's response had no review URL".into()))
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
fn check_pending_commit(pending: &PendingReview, commit_oid: &str) -> Result<(), CoreError> {
    if pending.commit_oid == commit_oid {
        return Ok(());
    }
    Err(CoreError::Api(format!(
        "your pending review on github.com is on commit {}, but you're reviewing {}. \
         Submit or cancel it on github.com, then try again.",
        short_oid(&pending.commit_oid),
        short_oid(commit_oid)
    )))
}

/// Drafts not already in the pending review, so a retry doesn't post them twice.
/// Side isn't compared: GitHub doesn't report it for review comments, and the
/// inferred one can be wrong for comments on unchanged lines.
fn new_drafts<'a>(drafts: &'a [DraftComment], pending: &[PendingComment]) -> Vec<&'a DraftComment> {
    // A single-line range is sent without a start line, whichever way it was stored.
    let start = |start: Option<u32>, line: Option<u32>| start.filter(|&s| Some(s) != line);
    drafts
        .iter()
        .filter(|draft| {
            !pending.iter().any(|comment| {
                comment.path == draft.path
                    && comment.line == Some(draft.line)
                    && start(comment.start_line, comment.line)
                        == start(draft.start_line, Some(draft.line))
                    && comment.body.trim() == draft.body.trim()
            })
        })
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
            base_ref_name: node.base_ref_name,
            head_ref_name: node.head_ref_name,
            head_oid: node.head_ref_oid,
            files,
            threads: threads
                .into_iter()
                .filter_map(|thread| convert_thread(thread).transpose())
                .collect::<Result<_, _>>()?,
            pending_review: viewer_pending_review(node.reviews),
        },
        files_after,
        threads_after,
    })
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
    // shortcut: only the first 100 comments of a pending review are read, so past that the count is low and a retry could repeat a draft; paginate if reviews that big show up.
    let comments = review.comments.nodes.into_iter().map(|comment| {
        // The hunk ends at the commented line, and only a removed line is on the left.
        let removed = comment
            .diff_hunk
            .lines()
            .last()
            .is_some_and(|line| line.starts_with('-'));
        PendingComment {
            path: comment.path,
            line: comment.line,
            start_line: comment.start_line,
            side: if removed {
                DiffSide::Left
            } else {
                DiffSide::Right
            },
            body: comment.body,
        }
    });
    PendingReview {
        id: review.id,
        commit_oid: review.commit.map(|commit| commit.oid).unwrap_or_default(),
        comments: comments.collect(),
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

    /// Deletes the PR's drafts and the record of an unfinished submit. A pending
    /// review that submit started stays on github.com, as yours.
    pub fn clear(&self, repo: String, number: u64) -> Result<(), CoreError> {
        write_file(&self.attempt_path(&repo, number)?, None)?;
        self.change(&repo, number, |drafts| {
            drafts.clear();
            Ok(())
        })
    }
}

impl DraftStore {
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
        write_file(&path, json)?;
        self.notify(repo, number);
        Ok(())
    }

    /// Settles a submit GitHub accepted: deletes the drafts it sent, unless
    /// edited since, then its record. Drafts added meanwhile stay.
    pub(crate) fn finish_attempt(
        &self,
        repo: &str,
        number: u64,
        attempt: &SubmitAttempt,
    ) -> Result<(), CoreError> {
        self.change(repo, number, |drafts| {
            drafts.retain(|draft| !attempt.drafts.contains(draft));
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
            save(&path, &edited)?;
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
fn save(path: &Path, drafts: &[DraftComment]) -> Result<(), CoreError> {
    let json = (!drafts.is_empty())
        .then(|| serde_json::to_vec_pretty(drafts).expect("drafts serialize to JSON"));
    write_file(path, json)
}

/// Writes a temp file and renames it over the old one, so a crash mid-write
/// can't leave a truncated file. `None` deletes the file.
fn write_file(path: &Path, json: Option<Vec<u8>>) -> Result<(), CoreError> {
    let storage =
        |e: io::Error| CoreError::Storage(format!("couldn't save {}: {e}", path.display()));
    let Some(json) = json else {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(storage(e)),
            _ => Ok(()),
        };
    };
    let tmp = path.with_extension("json.tmp");
    fs::create_dir_all(path.parent().expect("draft paths have a parent")).map_err(storage)?;
    let mut file = fs::File::create(&tmp).map_err(storage)?;
    file.write_all(&json)
        .and_then(|()| file.sync_all())
        .map_err(storage)?;
    fs::rename(&tmp, path).map_err(storage)
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
    use std::time::Duration;

    /// Shaped like a real first page: three files (more pending), an outdated
    /// thread, a multi-line thread, a comment from a deleted account, and the
    /// viewer's pending review (behind one that isn't theirs, which GitHub
    /// shouldn't return but is filtered anyway).
    const PR_RESPONSE: &str = r#"{
      "data": { "repository": { "pullRequest": {
        "id": "PR_kwDOAbc123", "title": "Add review window", "body": "Closes #4.",
        "url": "https://github.com/o/r/pull/42", "isDraft": false,
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
              { "path": "src/pr.rs", "line": 12, "startLine": 8, "body": "Split this up?",
                "diffHunk": "@@ -1,3 +1,12 @@\n fn a() {}\n+fn b() {}" },
              { "path": "src/lib.rs", "line": 4, "startLine": null, "body": "Why remove this?",
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
                  "createdAt": "2026-11-05T09:00:00Z", "url": "https://github.com/o/r/pull/42#discussion_r2" },
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
        side: DiffSide,
        body: &str,
    ) -> PendingComment {
        PendingComment {
            path: path.into(),
            line,
            start_line,
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
                    DiffSide::Right,
                    "Split this up?"
                ),
                pending(
                    "src/lib.rs",
                    Some(4),
                    None,
                    DiffSide::Left,
                    "Why remove this?"
                ),
                pending("README.md", None, None, DiffSide::Right, "Whole file."),
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
          "id": "PR_1", "title": "t", "body": "", "url": "u", "isDraft": true,
          "baseRefName": "main", "headRefName": "h", "headRefOid": "abc", "author": { "login": "me" },
          "reviewThreads": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
        } } } }"#;
        let page = parse_pr_page(body, "o/r", 1).unwrap();
        assert!(page.detail.files.is_empty());
        assert_eq!(page.detail.pending_review, None);
        assert_eq!((page.files_after, page.threads_after), (None, None));
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

    #[test]
    fn parses_the_recorded_review() {
        let pending = r#"{ "data": { "node": {
          "id": "PRR_1", "state": "PENDING", "url": "https://github.com/o/r/pull/7#pullrequestreview-9",
          "viewerDidAuthor": true, "commit": { "oid": "abc" },
          "comments": { "nodes": [
            { "path": "src/pr.rs", "line": 12, "startLine": null, "body": "comment 1",
              "diffHunk": "@@ -1,2 +1,12 @@\n+fn b() {}" } ] } } } }"#;
        let recorded = parse_recorded_review(pending).unwrap();
        assert_eq!(recorded.state, "PENDING");
        assert_eq!(
            recorded.url,
            "https://github.com/o/r/pull/7#pullrequestreview-9"
        );
        assert_eq!(recorded.review.id, "PRR_1");
        assert_eq!(recorded.review.commit_oid, "abc");
        assert_eq!(recorded.review.comments[0].body, "comment 1");
    }

    #[test]
    fn names_what_an_earlier_posted_try_left_out() {
        let sent = draft("1", None);
        let earlier = SubmitAttempt {
            review_id: "PRR_1".into(),
            commit_oid: "abc".into(),
            event: ReviewEvent::Approve,
            body: "LGTM".into(),
            drafts: vec![sent.clone()],
        };
        let submission = |event, body: &str, drafts: Vec<DraftComment>| Submission {
            pull_request_id: "PR_1".into(),
            commit_oid: "abc".into(),
            event,
            body: body.into(),
            drafts,
        };
        // The retry the user meant: nothing differs, so it's a success.
        assert!(
            unsent(
                &earlier,
                &submission(ReviewEvent::Approve, " LGTM\n", vec![sent.clone()])
            )
            .is_empty()
        );
        // A draft sent earlier and since removed here doesn't count either.
        assert!(unsent(&earlier, &submission(ReviewEvent::Approve, "LGTM", vec![])).is_empty());

        let mut edited = sent.clone();
        edited.body = "reworded".into();
        let mut late = draft("2", None);
        late.line = 30;
        let changed = submission(
            ReviewEvent::RequestChanges,
            "Needs work",
            vec![edited, late],
        );
        assert_eq!(
            unsent(&earlier, &changed),
            [
                "the Request Changes verdict",
                "this summary",
                "the drafts on src/pr.rs:12, src/pr.rs:30, which are still here",
            ]
        );
        assert_eq!(posted_as("APPROVED"), "an approval");
    }

    #[test]
    fn never_takes_a_recorded_review_for_deleted() {
        // GitHub answers a deleted id like this, but so could a passing failure: keep the record.
        let null_with_errors = r#"{ "data": { "node": null },
          "errors": [ { "type": "NOT_FOUND", "message": "Could not resolve to a node with the global id of 'PRR_1'" } ] }"#;
        assert!(
            parse_recorded_review(null_with_errors)
                .unwrap_err()
                .to_string()
                .contains("Could not resolve")
        );
        assert!(parse_recorded_review(r#"{ "data": { "node": null } }"#).is_err());
        let failed = r#"{ "errors": [ { "message": "API rate limit exceeded" } ] }"#;
        assert!(
            parse_recorded_review(failed)
                .unwrap_err()
                .to_string()
                .contains("rate limit")
        );
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
            // A retry after "1" and "2" went in. Bodies compare trimmed; sides aren't compared.
            pending("src/pr.rs", Some(12), None, DiffSide::Right, "comment 1\n"),
            pending("src/pr.rs", Some(12), Some(8), DiffSide::Right, "comment 2"),
            pending(
                "src/pr.rs",
                Some(12),
                None,
                DiffSide::Right,
                "comment single",
            ),
            pending(
                "src/pr.rs",
                Some(12),
                None,
                DiffSide::Right,
                "comment moved",
            ),
            pending(
                "src/pr.rs",
                Some(12),
                None,
                DiffSide::Right,
                "something else",
            ),
        ];
        let ids: Vec<_> = new_drafts(&drafts, &pending)
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(ids, ["edited", "moved"]);
        assert_eq!(new_drafts(&drafts, &[]).len(), 5);
    }

    #[test]
    fn refuses_a_pending_review_on_another_commit() {
        let pending = |commit: &str| PendingReview {
            id: "PRR_1".into(),
            commit_oid: commit.into(),
            comments: vec![],
        };
        let head = "6dcb09b5b57875f334f61aebed695e2e4193db5e";
        assert!(check_pending_commit(&pending(head), head).is_ok());
        let message = check_pending_commit(&pending("1234567890abcdef"), head)
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("on commit 1234567") && message.contains("reviewing 6dcb09b"),
            "{message}"
        );
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
            review_id: "PRR_1".into(),
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
        reopened.finish_attempt("o/r", 7, &attempt(vec![])).unwrap();
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
            .finish_attempt("o/r", 7, &store.attempt("o/r", 7).unwrap().unwrap())
            .unwrap();
        assert_eq!(store.list("o/r".into(), 7).unwrap(), [late]);
        assert_eq!(store.attempt("o/r", 7).unwrap(), None);
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

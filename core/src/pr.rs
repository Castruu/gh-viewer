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
/// only fetches what's left.
const PR_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!,
      $withFiles: Boolean!, $filesAfter: String, $withThreads: Boolean!, $threadsAfter: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id title body url isDraft baseRefName headRefName headRefOid
      author { login }
      # GitHub caps this connection at 3000 files.
      files(first: 100, after: $filesAfter) @include(if: $withFiles) {
        pageInfo { hasNextPage endCursor }
        nodes { path viewerViewedState }
      }
      reviewThreads(first: 100, after: $threadsAfter) @include(if: $withThreads) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id path line startLine diffSide startDiffSide isResolved isOutdated
          comments(first: 100) { nodes { author { login } body createdAt url } }
        }
      }
    }
  }
}
"#;

const MARK_VIEWED: &str = r#"
mutation($input: MarkFileAsViewedInput!) { markFileAsViewed(input: $input) { clientMutationId } }
"#;

const UNMARK_VIEWED: &str = r#"
mutation($input: UnmarkFileAsViewedInput!) { unmarkFileAsViewed(input: $input) { clientMutationId } }
"#;

const SUBMIT_REVIEW: &str = r#"
mutation($input: AddPullRequestReviewInput!) {
  addPullRequestReview(input: $input) { pullRequestReview { url } }
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
    /// The head commit; pass it to `submit_review` so the review lands on what you read.
    pub head_oid: String,
    pub files: Vec<ChangedFile>,
    pub threads: Vec<ReviewThread>,
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
    /// First line of a multi-line thread.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
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
        let mut variables = json!({
            "owner": owner, "name": name, "number": number,
            "withFiles": true, "withThreads": true,
        });

        let first = parse_pr_page(
            &self.graphql(PR_QUERY, variables.clone()).await?,
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
                &self.graphql(PR_QUERY, variables.clone()).await?,
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

    /// Creates and submits a review in one mutation; returns the review's URL.
    pub async fn submit_review(
        &self,
        pull_request_id: String,
        commit_oid: String,
        event: ReviewEvent,
        body: String,
        comments: Vec<DraftComment>,
    ) -> Result<String, CoreError> {
        let variables = review_variables(&pull_request_id, &commit_oid, event, &body, &comments);
        let data = parse_mutation(&self.graphql(SUBMIT_REVIEW, variables).await?)?;
        data.pointer("/addPullRequestReview/pullRequestReview/url")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| CoreError::Api("GitHub's response had no review URL".into()))
    }
}

/// Variables for `SUBMIT_REVIEW`: every draft becomes a `DraftPullRequestReviewThread`.
fn review_variables(
    pull_request_id: &str,
    commit_oid: &str,
    event: ReviewEvent,
    body: &str,
    comments: &[DraftComment],
) -> Value {
    let event = match event {
        ReviewEvent::Approve => "APPROVE",
        ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        ReviewEvent::Comment => "COMMENT",
    };
    let threads: Vec<Value> = comments
        .iter()
        .map(|comment| {
            let mut thread = json!({
                "path": comment.path, "line": comment.line, "side": comment.side, "body": comment.body,
            });
            if let Some(start_line) = comment.start_line {
                thread["startLine"] = start_line.into();
            }
            if let Some(start_side) = comment.start_side {
                thread["startSide"] = json!(start_side);
            }
            thread
        })
        .collect();
    json!({ "input": {
        "pullRequestId": pull_request_id, "commitOID": commit_oid,
        "event": event, "body": body, "threads": threads,
    } })
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
    // addPullRequestReview can't create a second review while one is pending,
    // e.g. one started on github.com and never submitted.
    if messages
        .iter()
        .any(|m| m.to_lowercase().contains("one pending review"))
    {
        return Err(CoreError::Api(format!(
            "you already have a pending review on this PR. Submit or cancel it on github.com, then try again ({})",
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
                .map(convert_thread)
                .collect::<Result<_, _>>()?,
        },
        files_after,
        threads_after,
    })
}

fn convert_thread(node: ThreadNode) -> Result<ReviewThread, CoreError> {
    let comments = node
        .comments
        .nodes
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
    Ok(ReviewThread {
        id: node.id,
        path: node.path,
        line: node.line,
        start_line: node.start_line,
        side: node.diff_side,
        start_side: node.start_diff_side,
        is_resolved: node.is_resolved,
        is_outdated: node.is_outdated,
        comments,
    })
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
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    // Thread comments are fetched without pageInfo.
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
        };
        self.change(&repo, number, |drafts| {
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

    /// Deletes the PR's drafts, e.g. once GitHub accepted the review.
    pub fn clear(&self, repo: String, number: u64) -> Result<(), CoreError> {
        self.change(&repo, number, |drafts| {
            drafts.clear();
            Ok(())
        })
    }
}

impl DraftStore {
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
        let listener = lock(&self.listener).clone();
        if let Some(listener) = listener {
            listener.drafts_changed(repo.to_owned(), number);
        }
        Ok(())
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

/// Writes a temp file and renames it over the old one, so a crash mid-write
/// can't leave a truncated file. No drafts means no file.
fn save(path: &Path, drafts: &[DraftComment]) -> Result<(), CoreError> {
    let storage = |e: io::Error| {
        CoreError::Storage(format!("couldn't save drafts to {}: {e}", path.display()))
    };
    if drafts.is_empty() {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(storage(e)),
            _ => Ok(()),
        };
    }
    let json = serde_json::to_vec_pretty(drafts).expect("drafts serialize to JSON");
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
    /// thread, a multi-line thread and a comment from a deleted account.
    const PR_RESPONSE: &str = r#"{
      "data": { "repository": { "pullRequest": {
        "id": "PR_kwDOAbc123", "title": "Add review window", "body": "Closes #4.",
        "url": "https://github.com/o/r/pull/42", "isDraft": false,
        "baseRefName": "main", "headRefName": "feat/review",
        "headRefOid": "6dcb09b5b57875f334f61aebed695e2e4193db5e",
        "author": null,
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
                { "author": { "login": "octocat" }, "body": "Typo here.",
                  "createdAt": "2026-11-04T20:57:47Z", "url": "https://github.com/o/r/pull/42#discussion_r1" }
              ] }
            },
            {
              "id": "PRRT_multi", "path": "src/pr.rs", "line": 12, "startLine": 8,
              "diffSide": "RIGHT", "startDiffSide": "LEFT", "isResolved": false, "isOutdated": false,
              "comments": { "nodes": [
                { "author": null, "body": "Can this be one function?",
                  "createdAt": "2026-11-05T09:00:00Z", "url": "https://github.com/o/r/pull/42#discussion_r2" },
                { "author": { "login": "hubot" }, "body": "Done.",
                  "createdAt": "2026-11-05T10:30:00Z", "url": "https://github.com/o/r/pull/42#discussion_r3" }
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

    #[test]
    fn parses_review_threads() {
        let threads = parse_pr_page(PR_RESPONSE, "o/r", 42)
            .unwrap()
            .detail
            .threads;
        assert_eq!(threads.len(), 2);

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
        }
    }

    #[test]
    fn builds_review_variables() {
        let comments = [draft("1", None), draft("2", Some((8, DiffSide::Left)))];
        let variables = review_variables(
            "PR_1",
            "abc",
            ReviewEvent::RequestChanges,
            "Needs work",
            &comments,
        );
        assert_eq!(
            variables,
            json!({ "input": {
            "pullRequestId": "PR_1",
            "commitOID": "abc",
            "event": "REQUEST_CHANGES",
            "body": "Needs work",
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
        let event =
            |event| review_variables("PR_1", "abc", event, "", &[])["input"]["event"].clone();
        assert_eq!(event(ReviewEvent::Approve), "APPROVE");
        assert_eq!(event(ReviewEvent::Comment), "COMMENT");
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
            message.contains("you already have a pending review"),
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
    }
}

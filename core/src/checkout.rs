//! App-managed clones, one worktree per PR, and the local diff.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::process::Command;

use crate::{CoreError, DiffSide, gh_path};

/// Finder-launched apps get a minimal PATH, so look where Homebrew and the
/// Command Line Tools put git before falling back to a PATH lookup.
const GIT_CANDIDATES: &[&str] = &[
    "/opt/homebrew/bin/git",
    "/usr/local/bin/git",
    "/usr/bin/git",
];

/// Owns the bare clones and per-PR worktrees under the app's data dir:
/// `repos/<owner>/<name>.git` and `worktrees/<owner>/<name>/<number>`.
#[derive(uniffi::Object)]
pub struct CheckoutManager {
    root: String,
    /// Remote URLs are `<remote_base><owner>/<name>.git`.
    remote_base: String,
    /// One lock per repo: git's own lock files make concurrent clones and
    /// fetches of the same repo fail instead of waiting.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// A PR checked out locally.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct Checkout {
    /// "owner/name"
    pub repo: String,
    pub number: u64,
    /// Detached worktree at `head_oid`, where the agent works. It may have local edits.
    pub worktree_path: String,
    /// The commit that's checked out and diffed: the pinned head if there is one.
    pub head_oid: String,
    /// The PR head as just fetched. Differs from `head_oid` only when pinned and
    /// new commits were pushed since.
    pub latest_head_oid: String,
    /// Tip of the base branch when it was fetched.
    pub base_oid: String,
    /// What the diff starts from, as in GitHub's three-dot "Files changed" diff.
    pub merge_base_oid: String,
}

/// One file in a PR's diff.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DiffFile {
    /// The new path, or the old one for a deleted file.
    pub path: String,
    /// The path before a rename; `None` for every other status.
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// Binary files have no hunks.
    pub is_binary: bool,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FileStatus {
    Added,
    /// Includes mode-only changes, which have no hunks.
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DiffHunk {
    /// The whole `@@ -1,4 +1,5 @@ fn main()` line.
    pub header: String,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line number in the base; `None` for added lines.
    pub old_line: Option<u32>,
    /// Line number in the head; `None` for removed lines.
    pub new_line: Option<u32>,
    /// The line without its leading `+`, `-` or space and without the newline.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[uniffi::export(async_runtime = "tokio")]
impl CheckoutManager {
    /// `root` is the app's data dir; Swift passes ~/Library/Application Support/GHViewer.
    #[uniffi::constructor]
    pub fn new(root: String) -> Self {
        Self::with_remote_base(root, "https://github.com/".into())
    }

    /// Clones `repo` ("owner/name") if needed, fetches the PR head and the base branch,
    /// and moves the PR's worktree to the head.
    ///
    /// `pin_head` is the full id of the commit the PR's unsent drafts were written
    /// on. When set, that commit is checked out and diffed instead of the latest
    /// head, and kept alive with a ref; `None` drops the pin.
    pub async fn prepare(
        &self,
        repo: String,
        number: u64,
        base_ref: String,
        pin_head: Option<String>,
    ) -> Result<Checkout, CoreError> {
        let paths = self.paths(&repo, number)?;
        // These would change what the refspecs below mean; git rejects other bad names itself.
        if base_ref.is_empty() || base_ref.contains([':', '*']) {
            return Err(CoreError::Git(format!("invalid base branch {base_ref:?}")));
        }
        if let Some(pin) = pin_head.as_deref().filter(|pin| !is_oid(pin)) {
            return Err(CoreError::Git(format!("invalid pinned commit {pin:?}")));
        }
        let lock = self.lock_for(&repo);
        let _guard = lock.lock().await;

        if !Path::new(&paths.bare).exists() {
            // Clone beside the final path and rename, so a clone cut short (crash,
            // power loss) is never mistaken for a complete one.
            let partial = format!("{}.partial", paths.bare);
            if Path::new(&partial).exists() {
                remove_dir(&partial).await?;
            }
            git(&[
                "clone",
                "--quiet",
                "--bare",
                "--filter=blob:none",
                &paths.url,
                &partial,
            ])
            .await
            .map_err(remote_error(&repo, format!("Couldn't clone {repo}")))?;
            tokio::fs::rename(&partial, &paths.bare)
                .await
                .map_err(|e| CoreError::Storage(format!("couldn't move {partial}: {e}")))?;
        }

        // A bare clone has no fetch refspec of its own. Fetching into a private
        // namespace means nothing here can clobber anything else.
        let pull_ref = format!("refs/ghv/pull/{number}");
        let base_local = format!("refs/ghv/base/{base_ref}");
        let pull_spec = format!("+refs/pull/{number}/head:{pull_ref}");
        let base_spec = format!("+refs/heads/{base_ref}:{base_local}");
        git(&[
            "-C",
            &paths.bare,
            "fetch",
            "--quiet",
            "--no-tags",
            "origin",
            &pull_spec,
            &base_spec,
        ])
        .await
        .map_err(remote_error(
            &repo,
            format!("Couldn't fetch PR #{number} and branch {base_ref} of {repo}"),
        ))?;

        let oids = git(&["-C", &paths.bare, "rev-parse", &pull_ref, &base_local])
            .await
            .map_err(|e| CoreError::Git(format!("Couldn't read the fetched commits: {e}")))?;
        let mut oids = oids.lines().map(str::to_string);
        let (latest_head_oid, base_oid) = (
            oids.next().unwrap_or_default(),
            oids.next().unwrap_or_default(),
        );

        // The ref keeps a pinned commit from being garbage-collected after a force-push.
        let pinned_ref = format!("refs/ghv/pinned/{number}");
        let head_oid = match pin_head {
            None => {
                git(&["-C", &paths.bare, "update-ref", "-d", &pinned_ref])
                    .await
                    .map_err(|e| CoreError::Git(format!("Couldn't unpin PR #{number}: {e}")))?;
                latest_head_oid.clone()
            }
            Some(pin) => {
                // Usually still here as the head fetched before a force-push. If not, fetch
                // it by id; git skips the download when it's present.
                let pin_spec = format!("+{pin}:{pinned_ref}");
                git(&[
                    "-C",
                    &paths.bare,
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "origin",
                    &pin_spec,
                ])
                .await
                .map_err(|e| {
                    CoreError::Git(format!(
                        "The commit your drafts on PR #{number} were written on ({}) is no \
                             longer available on GitHub, probably after a force-push. ({e})",
                        &pin[..7]
                    ))
                })?;
                pin
            }
        };

        let merge_base_oid = git(&["-C", &paths.bare, "merge-base", &base_oid, &head_oid])
            .await
            .map_err(|e| {
                CoreError::Git(format!(
                    "PR #{number} shares no history with {base_ref}: {e}"
                ))
            })?
            .trim()
            .to_string();

        let wt = &paths.worktree;
        // `None` also when the worktree's repo is gone, e.g. repos/ was deleted by hand.
        let current = if Path::new(wt).join(".git").exists() {
            git(&["-C", wt, "rev-parse", "HEAD"]).await.ok()
        } else {
            None
        };
        let checkout_error =
            || remote_error(&repo, format!("Couldn't check out PR #{number} of {repo}"));
        match current {
            // Already there: keep whatever the agent changed.
            Some(oid) if oid.trim() == head_oid => {}
            // Local edits are discarded on purpose: the worktree is the agent's scratch
            // space. No `git clean`, so untracked build caches survive.
            Some(_) => {
                git(&[
                    "-C", wt, "checkout", "--quiet", "--force", "--detach", &head_oid,
                ])
                .await
                .map_err(checkout_error())?;
            }
            None => {
                if Path::new(wt).exists() {
                    remove_dir(wt).await?;
                }
                // Forget worktrees whose directories are gone, or `add` refuses the path.
                git(&["-C", &paths.bare, "worktree", "prune"])
                    .await
                    .map_err(|e| {
                        CoreError::Git(format!("Couldn't prune worktrees of {repo}: {e}"))
                    })?;
                git(&[
                    "-C",
                    &paths.bare,
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    wt,
                    &head_oid,
                ])
                .await
                .map_err(checkout_error())?;
            }
        }

        Ok(Checkout {
            repo,
            number,
            worktree_path: paths.worktree,
            head_oid,
            latest_head_oid,
            base_oid,
            merge_base_oid,
        })
    }

    /// The PR's diff, merge base → head, matching GitHub's "Files changed".
    pub async fn diff(&self, checkout: Checkout) -> Result<Vec<DiffFile>, CoreError> {
        let bare = self.paths(&checkout.repo, checkout.number)?.bare;
        let (base, head) = (&checkout.merge_base_oid, &checkout.head_oid);
        if !is_oid(base) || !is_oid(head) {
            return Err(CoreError::Git(format!(
                "invalid commits {base:?}..{head:?}"
            )));
        }
        // Between commits, never against the worktree: the agent's edits there aren't part of the PR.
        let out = git(&[
            "-C",
            &bare,
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "-M",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            base,
            head,
        ])
        .await
        // Missing blobs are fetched from GitHub here, so this can fail like a fetch.
        .map_err(remote_error(
            &checkout.repo,
            format!("Couldn't diff PR #{} of {}", checkout.number, checkout.repo),
        ))?;
        Ok(parse_diff(&out))
    }
}

/// Where a repo lives locally and remotely.
struct Paths {
    url: String,
    bare: String,
    worktree: String,
}

impl CheckoutManager {
    /// Like `new`, but serves repos from `<remote_base><owner>/<name>.git`, e.g. a
    /// `file://` directory in tests.
    pub(crate) fn with_remote_base(root: String, remote_base: String) -> Self {
        Self {
            root,
            remote_base,
            locks: Mutex::default(),
        }
    }

    /// Checks that `repo` is a plain "owner/name", since it becomes part of paths.
    fn paths(&self, repo: &str, number: u64) -> Result<Paths, CoreError> {
        let valid = |part: &str| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        let Some((owner, name)) = repo.split_once('/').filter(|(o, n)| valid(o) && valid(n)) else {
            return Err(CoreError::Git(format!(
                "invalid repo {repo:?}, expected \"owner/name\""
            )));
        };
        Ok(Paths {
            url: format!("{}{owner}/{name}.git", self.remote_base),
            bare: format!("{}/repos/{owner}/{name}.git", self.root),
            worktree: format!("{}/worktrees/{owner}/{name}/{number}", self.root),
        })
    }

    fn lock_for(&self, repo: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .expect("nothing panics while holding the lock map");
        // GitHub names are case-insensitive, and so is the default macOS file system.
        locks.entry(repo.to_ascii_lowercase()).or_default().clone()
    }
}

/// A full commit id as git prints it. Ids are passed as arguments, so this also
/// makes sure none can be read as an option.
fn is_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Path to the git binary, or plain "git" for a PATH lookup.
fn git_path() -> &'static str {
    GIT_CANDIDATES
        .iter()
        .copied()
        .find(|path| Path::new(path).exists())
        .unwrap_or("git")
}

/// Runs git with auth through gh and without the user's config. Returns stdout,
/// or stderr as the error.
async fn git(args: &[&str]) -> Result<String, String> {
    let helper = format!("credential.helper=!{} auth git-credential", gh_path());
    let output = Command::new(git_path())
        // An empty helper clears the list, so only gh answers and no token is stored
        // anywhere. `-c` also reaches the git processes that fetch missing blobs.
        .args([
            "-c",
            "credential.helper=",
            "-c",
            &helper,
            "-c",
            "core.quotePath=false",
        ])
        // A stalled connection (sleep, VPN switch) fails after two minutes without a
        // byte instead of hanging forever. A wall-clock limit would cut big clones short.
        .args(["-c", "http.lowSpeedLimit=1", "-c", "http.lowSpeedTime=120"])
        .args(args)
        // Ignore ~/.gitconfig and the system config. Their url rewrites (to ssh), hooks,
        // LFS filters and diff options would break fetching, checkout or the diff parser.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("couldn't run {}: {e}", git_path()))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        output.status.to_string()
    } else {
        stderr
    })
}

/// Maps a failed git command that talked to GitHub, explaining the usual cause
/// when the repo can't be read.
fn remote_error(repo: &str, context: String) -> impl FnOnce(String) -> CoreError {
    let repo = repo.to_string();
    move |stderr| {
        // GitHub says "not found" for private repos the token can't read, and asks
        // for a username (which fails with prompts off) when there's no token at all.
        if stderr.contains("Repository not found") || stderr.contains("could not read Username") {
            CoreError::Git(format!(
                "Can't access {repo} on GitHub. If it's private, check that the account in \
                 `gh auth status` can read it and has SSO authorized for its org. ({stderr})"
            ))
        } else {
            CoreError::Git(format!("{context}: {stderr}"))
        }
    }
}

async fn remove_dir(path: &str) -> Result<(), CoreError> {
    tokio::fs::remove_dir_all(path)
        .await
        .map_err(|e| CoreError::Storage(format!("couldn't remove {path}: {e}")))
}

/// Parses `git diff` output. Hunk lines are read by count, so content such as
/// `--- x` inside a hunk is never taken for a header.
fn parse_diff(out: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    // Lines the current hunk still has on each side, and the next line numbers.
    let (mut old_left, mut new_left, mut old_line, mut new_line) = (0u32, 0u32, 0, 0);
    for line in out.split('\n') {
        if old_left > 0 || new_left > 0 {
            let kind = match line.as_bytes().first() {
                // A blank context line is just "" under diff.suppressBlankEmpty.
                Some(b' ') | None => Some(LineKind::Context),
                Some(b'-') => Some(LineKind::Removed),
                Some(b'+') => Some(LineKind::Added),
                // "\ No newline at end of file" is about the line before it.
                Some(b'\\') => continue,
                _ => None,
            };
            if let (Some(kind), Some(hunk)) =
                (kind, files.last_mut().and_then(|f| f.hunks.last_mut()))
            {
                let old = (kind != LineKind::Added).then_some(old_line);
                let new = (kind != LineKind::Removed).then_some(new_line);
                if old.is_some() {
                    old_line += 1;
                    old_left = old_left.saturating_sub(1);
                }
                if new.is_some() {
                    new_line += 1;
                    new_left = new_left.saturating_sub(1);
                }
                let text = line.get(1..).unwrap_or_default().to_string();
                hunk.lines.push(DiffLine {
                    kind,
                    old_line: old,
                    new_line: new,
                    text,
                });
                continue;
            }
            // Malformed hunk: read this line as a header instead.
            (old_left, new_left) = (0, 0);
        }

        if let Some(rest) = line.strip_prefix("diff --git ") {
            files.push(DiffFile {
                path: git_header_path(rest),
                old_path: None,
                status: FileStatus::Modified,
                is_binary: false,
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if let Some(hunk) = parse_hunk_header(line) {
            (old_line, new_line) = (hunk.old_start, hunk.new_start);
            (old_left, new_left) = (hunk.old_count, hunk.new_count);
            file.hunks.push(hunk);
        } else if line.starts_with("new file mode ") {
            file.status = FileStatus::Added;
        } else if line.starts_with("deleted file mode ") {
            file.status = FileStatus::Deleted;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = Some(unquote_path(path));
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.path = unquote_path(path);
        } else if let Some(path) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        {
            // `+++` comes second, so a deleted file keeps its old path and the rest
            // get their new one. Git ends the line with a tab when the path has a space.
            if path != "/dev/null" {
                let path = unquote_path(path.strip_suffix('\t').unwrap_or(path));
                // Drop the `a/` or `b/` prefix.
                file.path = path.get(2..).unwrap_or_default().to_string();
            }
        } else if line.starts_with("Binary files ") {
            file.is_binary = true;
        }
    }
    files
}

/// The path on a `diff --git a/<path> b/<path>` line. It's only the fallback for
/// files without `---`/`+++` lines (binary, empty, mode-only). Renames, the one
/// case where the two paths differ, set the path from `rename to`.
fn git_header_path(rest: &str) -> String {
    let b_side = match unquote(rest) {
        // `"a/<path>" "b/<path>"`
        Some((_, tail)) => unquote(tail.trim_start()).map(|(b, _)| b),
        // Both halves are equal, so `b/<path>` is the second half.
        None => rest.get(rest.len().div_ceil(2)..).map(str::to_string),
    };
    let b_side = b_side.unwrap_or_else(|| rest.to_string());
    b_side.strip_prefix("b/").unwrap_or(&b_side).to_string()
}

/// A path as git writes it in headers: C-quoted when it has special characters.
fn unquote_path(path: &str) -> String {
    unquote(path).map_or_else(|| path.to_string(), |(path, _)| path)
}

/// Decodes a C-quoted string at the start of `s` (`"tab\there"`), returning it and
/// the rest of `s`. `None` if `s` doesn't start with a well-formed quoted string.
fn unquote(s: &str) -> Option<(String, &str)> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), &s[i + 1..])),
            b'\\' => {
                i += 1;
                match *bytes.get(i)? {
                    // Octal byte, e.g. `\303\251` for "é" when core.quotePath is on.
                    b'0'..=b'7' => {
                        out.push(u8::from_str_radix(s.get(i..i + 3)?, 8).ok()?);
                        i += 2;
                    }
                    b'a' => out.push(0x07),
                    b'b' => out.push(0x08),
                    b't' => out.push(b'\t'),
                    b'n' => out.push(b'\n'),
                    b'v' => out.push(0x0b),
                    b'f' => out.push(0x0c),
                    b'r' => out.push(b'\r'),
                    other => out.push(other), // `\"` and `\\`
                }
            }
            other => out.push(other),
        }
        i += 1;
    }
    None
}

/// Parses `@@ -old_start[,old_count] +new_start[,new_count] @@ ...`; a missing count means 1.
fn parse_hunk_header(line: &str) -> Option<DiffHunk> {
    let (ranges, _) = line.strip_prefix("@@ -")?.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |range: &str| -> Option<(u32, u32)> {
        match range.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    };
    let ((old_start, old_count), (new_start, new_count)) = (range(old)?, range(new)?);
    Some(DiffHunk {
        header: line.to_string(),
        old_start,
        old_count,
        new_start,
        new_count,
        lines: Vec::new(),
    })
}

/// Whether GitHub would accept a review comment on `line` of `path` on `side`.
/// The line has to be inside a hunk on that side: Left takes old line numbers
/// of removed or context lines, Right takes new line numbers of added or context lines.
#[allow(dead_code)] // first used by mcp.rs
pub(crate) fn is_commentable(files: &[DiffFile], path: &str, side: DiffSide, line: u32) -> bool {
    // Added lines have no old number and removed lines no new one, so the number alone decides.
    files
        .iter()
        .filter(|file| file.path == path)
        .flat_map(|file| &file.hunks)
        .flat_map(|hunk| &hunk.lines)
        .any(|l| match side {
            DiffSide::Left => l.old_line == Some(line),
            DiffSide::Right => l.new_line == Some(line),
        })
}

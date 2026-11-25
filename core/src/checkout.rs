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
    /// Both set when the change alters the file's mode, e.g. "100644" → "100755"
    /// to make it executable. A mode-only change has no hunks.
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
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
        // On every prepare, so worktrees made before this lose them too.
        if let Err(error) = hide_agent_config(wt).await {
            let version = git(&["--version"]).await.unwrap_or_default();
            return Err(match too_old_for_sparse(&version) {
                Some(version) => CoreError::Git(format!(
                    "GH Viewer needs git {}.{} or newer to keep a PR's agent config out of its checkout (`git sparse-checkout set --no-cone`), but {} is version {version}. Update it, e.g. with `brew upgrade git`, then try again.",
                    SPARSE_MIN_GIT.0,
                    SPARSE_MIN_GIT.1,
                    git_path()
                )),
                None => checkout_error()(error),
            });
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
/// Keeps the files agents load from their cwd as config out of the worktree `wt`:
/// hooks, MCP servers and permissions that the PR's author would run on this
/// machine. The diff is between commits, so it still shows them.
pub(crate) async fn hide_agent_config(wt: &str) -> Result<String, String> {
    // The same names as AGENT_CONFIG.
    // No trailing slash: a PR's `.codex` may be a symlink or a file, not only a directory.
    git(&[
        "-C", wt, "sparse-checkout", "set", "--no-cone",
        "/*", "!/.claude", "!/.codex", "!/.mcp.json",
    ])
    .await
}

/// The first git whose `sparse-checkout set` takes `--no-cone`.
const SPARSE_MIN_GIT: (u32, u32) = (2, 35);

/// The version in `git --version` output, if it's older than [`SPARSE_MIN_GIT`].
fn too_old_for_sparse(output: &str) -> Option<String> {
    let version = output.trim().strip_prefix("git version ")?.split(' ').next()?;
    let mut parts = version.split('.').map(|part| part.parse::<u32>().ok());
    let (Some(major), Some(minor)) = (parts.next()?, parts.next()?) else {
        return None;
    };
    ((major, minor) < SPARSE_MIN_GIT).then(|| version.to_owned())
}

/// What agents read as config from the root of their cwd.
const AGENT_CONFIG: [&str; 3] = [".claude", ".codex", ".mcp.json"];

/// Removes agent config from the root of the worktree `wt`, whatever its type.
/// The sparse checkout only governs tracked files, so this catches config an
/// agent wrote. Symlinks are removed, not followed. Fails if any is left.
pub(crate) fn remove_agent_config(wt: &str) -> Result<(), CoreError> {
    for name in AGENT_CONFIG {
        let path = Path::new(wt).join(name);
        let removed = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path),
            Ok(_) => std::fs::remove_file(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        let gone = matches!(std::fs::symlink_metadata(&path), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        if !gone {
            let reason = removed.err().map(|e| format!(" ({e})")).unwrap_or_default();
            return Err(CoreError::Agent(format!(
                "Couldn't remove `{name}` from the PR's worktree{reason}. An agent would load it as its own config. Delete {}, then try again.",
                path.display()
            )));
        }
    }
    Ok(())
}

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
                old_mode: None,
                new_mode: None,
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
        } else if let Some(mode) = line.strip_prefix("old mode ") {
            file.old_mode = Some(mode.to_string());
        } else if let Some(mode) = line.strip_prefix("new mode ") {
            file.new_mode = Some(mode.to_string());
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
pub(crate) fn is_commentable(files: &[DiffFile], path: &str, side: DiffSide, line: u32) -> bool {
    hunk_of(files, path, side, line).is_some()
}

/// The index of the hunk holding a commentable line. GitHub takes a
/// multi-line comment only within one hunk.
pub(crate) fn hunk_of(files: &[DiffFile], path: &str, side: DiffSide, line: u32) -> Option<usize> {
    // Added lines have no old number and removed lines no new one, so the number alone decides.
    files
        .iter()
        .filter(|file| file.path == path)
        .flat_map(|file| file.hunks.iter().enumerate())
        .find(|(_, hunk)| {
            hunk.lines.iter().any(|l| match side {
                DiffSide::Left => l.old_line == Some(line),
                DiffSide::Right => l.new_line == Some(line),
            })
        })
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use LineKind::{Added, Context, Removed};

    /// Real output of the `diff` command above, with `<TAB>` for the tab git adds
    /// after `---`/`+++` paths that contain a space.
    const REAL_DIFF: &str = r#"diff --git a/café.txt b/café.txt
index ea17b16..572eb43 100644
--- a/café.txt
+++ b/café.txt
@@ -1 +1 @@
-cafe
+café
diff --git a/deleted.txt b/deleted.txt
deleted file mode 100644
index d3bc718..0000000
--- a/deleted.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-gone
-bye
diff --git a/empty.txt b/empty.txt
new file mode 100644
index 0000000..e69de29
diff --git a/image.bin b/image.bin
new file mode 100644
index 0000000..36b8a98
Binary files /dev/null and b/image.bin differ
diff --git a/modified.txt b/modified.txt
index 4e258e3..ada7c2d 100644
--- a/modified.txt
+++ b/modified.txt
@@ -1,5 +1,5 @@
 l1
-l2
+L2
 l3
 l4
 l5
@@ -7,6 +7,6 @@ l6
 l7
 l8
 l9
--- dashes
+++ pluses
 l11
-last
\ No newline at end of file
+LAST
\ No newline at end of file
diff --git a/pure.txt b/moved/pure.txt
similarity index 100%
rename from pure.txt
rename to moved/pure.txt
diff --git a/old name.txt b/new name.txt
similarity index 83%
rename from old name.txt
rename to new name.txt
index 0fdf397..e0318ee 100644
--- a/old name.txt<TAB>
+++ b/new name.txt<TAB>
@@ -3,4 +3,4 @@ b
 c
 d
 e
-f
+F
diff --git a/one.txt b/one.txt
index 5626abf..e438487 100644
--- a/one.txt
+++ b/one.txt
@@ -1 +1 @@
-one
+uno
diff --git "a/q\"uote.txt" "b/q\"uote.txt"
new file mode 100644
index 0000000..8e8a849
--- /dev/null
+++ "b/q\"uote.txt"
@@ -0,0 +1 @@
+q"uote
diff --git a/script.sh b/script.sh
old mode 100644
new mode 100755
diff --git "a/tab\tfile.txt" "b/tab\tfile.txt"
index 587be6b..975fbec 100644
--- "a/tab\tfile.txt"
+++ "b/tab\tfile.txt"
@@ -1 +1 @@
-x
+y
"#;

    fn real_diff() -> Vec<DiffFile> {
        parse_diff(&REAL_DIFF.replace("<TAB>", "\t"))
    }

    fn file<'a>(files: &'a [DiffFile], path: &str) -> &'a DiffFile {
        files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("no {path:?} in the diff"))
    }

    fn line(kind: LineKind, old_line: Option<u32>, new_line: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_line,
            new_line,
            text: text.into(),
        }
    }

    #[test]
    fn parses_every_file() {
        let paths: Vec<_> = real_diff().into_iter().map(|f| f.path).collect();
        assert_eq!(
            paths,
            [
                "café.txt",
                "deleted.txt",
                "empty.txt",
                "image.bin",
                "modified.txt",
                "moved/pure.txt",
                "new name.txt",
                "one.txt",
                "q\"uote.txt",
                "script.sh",
                "tab\tfile.txt",
            ]
        );
    }

    #[test]
    fn parses_multiple_hunks_with_header_like_content_and_no_newline_markers() {
        let files = real_diff();
        let modified = file(&files, "modified.txt");
        assert_eq!(modified.status, FileStatus::Modified);
        assert_eq!(modified.old_path, None);
        assert_eq!(modified.hunks.len(), 2);

        let first = &modified.hunks[0];
        assert_eq!(first.header, "@@ -1,5 +1,5 @@");
        assert_eq!(
            first.lines[..3],
            [
                line(Context, Some(1), Some(1), "l1"),
                line(Removed, Some(2), None, "l2"),
                line(Added, None, Some(2), "L2"),
            ]
        );

        let second = &modified.hunks[1];
        assert_eq!(second.header, "@@ -7,6 +7,6 @@ l6");
        assert_eq!(
            (
                second.old_start,
                second.old_count,
                second.new_start,
                second.new_count
            ),
            (7, 6, 7, 6)
        );
        assert_eq!(
            second.lines,
            [
                line(Context, Some(7), Some(7), "l7"),
                line(Context, Some(8), Some(8), "l8"),
                line(Context, Some(9), Some(9), "l9"),
                line(Removed, Some(10), None, "-- dashes"),
                line(Added, None, Some(10), "++ pluses"),
                line(Context, Some(11), Some(11), "l11"),
                line(Removed, Some(12), None, "last"),
                line(Added, None, Some(12), "LAST"),
            ]
        );
    }

    #[test]
    fn reads_omitted_hunk_counts_as_one() {
        let files = real_diff();
        let hunk = &file(&files, "one.txt").hunks[0];
        assert_eq!(
            (
                hunk.old_start,
                hunk.old_count,
                hunk.new_start,
                hunk.new_count
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(
            hunk.lines,
            [
                line(Removed, Some(1), None, "one"),
                line(Added, None, Some(1), "uno")
            ]
        );
        // Non-ASCII paths stay unquoted with core.quotePath=false.
        assert_eq!(file(&files, "café.txt").hunks[0].lines[1].text, "café");
    }

    #[test]
    fn parses_added_and_deleted_files() {
        let files = real_diff();
        let deleted = file(&files, "deleted.txt");
        assert_eq!(deleted.status, FileStatus::Deleted);
        assert_eq!(deleted.old_path, None);
        assert_eq!(
            deleted.hunks[0].lines,
            [
                line(Removed, Some(1), None, "gone"),
                line(Removed, Some(2), None, "bye"),
            ]
        );

        let empty = file(&files, "empty.txt");
        assert_eq!(
            (empty.status, empty.is_binary, empty.hunks.len()),
            (FileStatus::Added, false, 0)
        );

        let quoted = file(&files, "q\"uote.txt");
        assert_eq!(quoted.status, FileStatus::Added);
        assert_eq!(
            quoted.hunks[0].lines,
            [line(Added, None, Some(1), "q\"uote")]
        );
    }

    #[test]
    fn parses_binary_and_mode_only_changes() {
        let files = real_diff();
        let image = file(&files, "image.bin");
        assert_eq!(
            (image.status, image.is_binary, image.hunks.len()),
            (FileStatus::Added, true, 0)
        );

        let script = file(&files, "script.sh");
        assert_eq!(
            (script.status, script.is_binary, script.hunks.len()),
            (FileStatus::Modified, false, 0)
        );
        assert_eq!(
            (script.old_mode.as_deref(), script.new_mode.as_deref()),
            (Some("100644"), Some("100755"))
        );
        assert_eq!(file(&files, "one.txt").new_mode, None);
    }

    #[test]
    fn parses_renames() {
        let files = real_diff();
        let pure = file(&files, "moved/pure.txt");
        assert_eq!(pure.status, FileStatus::Renamed);
        assert_eq!(pure.old_path.as_deref(), Some("pure.txt"));
        assert!(pure.hunks.is_empty());

        let edited = file(&files, "new name.txt");
        assert_eq!(edited.status, FileStatus::Renamed);
        assert_eq!(edited.old_path.as_deref(), Some("old name.txt"));
        assert_eq!(edited.hunks[0].header, "@@ -3,4 +3,4 @@ b");
        assert_eq!(
            edited.hunks[0].lines[3..],
            [
                line(Removed, Some(6), None, "f"),
                line(Added, None, Some(6), "F"),
            ]
        );
    }

    #[test]
    fn unquotes_c_quoted_paths() {
        let files = real_diff();
        assert_eq!(file(&files, "tab\tfile.txt").hunks[0].lines.len(), 2);

        // As git writes them with core.quotePath on: octal bytes, escaped quotes and backslashes.
        let diff = r#"diff --git "a/caf\303\251 \"old\".txt" "b/caf\303\251 \\new\\.txt"
similarity index 100%
rename from "caf\303\251 \"old\".txt"
rename to "caf\303\251 \\new\\.txt"
diff --git "a/bin\tary" "b/bin\tary"
index 1234567..89abcde 100644
Binary files "a/bin\tary" and "b/bin\tary" differ
"#;
        let files = parse_diff(diff);
        assert_eq!(files[0].path, r"café \new\.txt");
        assert_eq!(files[0].old_path.as_deref(), Some("café \"old\".txt"));
        assert_eq!(files[0].status, FileStatus::Renamed);
        // The `diff --git` line is the only source of the path here.
        assert_eq!(files[1].path, "bin\tary");
        assert!(files[1].is_binary);
    }

    #[test]
    fn falls_back_to_the_git_header_for_paths_with_spaces() {
        let diff = "diff --git a/x b/y b/x b/y\nold mode 100644\nnew mode 100755\n";
        assert_eq!(parse_diff(diff)[0].path, "x b/y");
    }

    #[test]
    fn parses_an_empty_diff() {
        assert!(parse_diff("").is_empty());
    }

    #[test]
    fn commentable_lines_follow_github_sides() {
        use DiffSide::{Left, Right};
        let files = real_diff();
        let ok = |path, side, line| is_commentable(&files, path, side, line);

        // Context lines are commentable on both sides, changed lines only on their own.
        assert!(ok("modified.txt", Left, 1) && ok("modified.txt", Right, 1));
        assert!(ok("modified.txt", Left, 2) && ok("modified.txt", Right, 2));
        assert!(ok("modified.txt", Left, 10) && ok("modified.txt", Right, 10));
        // Line 6 sits between the two hunks; 13 is past the end.
        assert!(!ok("modified.txt", Right, 6) && !ok("modified.txt", Left, 6));
        assert!(!ok("modified.txt", Right, 13));

        assert!(ok("deleted.txt", Left, 1) && !ok("deleted.txt", Right, 1));
        assert!(ok("q\"uote.txt", Right, 1) && !ok("q\"uote.txt", Left, 1));
        // Renamed files take the new path on both sides.
        assert!(ok("new name.txt", Left, 6) && ok("new name.txt", Right, 3));
        assert!(!ok("old name.txt", Left, 6));
        assert!(!ok("image.bin", Right, 1) && !ok("script.sh", Right, 1));
        assert!(!ok("missing.txt", Right, 1));
    }

    #[test]
    fn rejects_repos_that_arent_owner_slash_name() {
        let manager = CheckoutManager::new("/data".into());
        for repo in [
            "", "a", "a/", "/b", "a/b/c", "../b", "a/..", "a/.", "a b/c", "a\\b/c",
        ] {
            assert!(
                manager.paths(repo, 1).is_err(),
                "{repo:?} should be rejected"
            );
        }
        let paths = manager.paths("rust-lang/rust.vim", 7).unwrap();
        assert_eq!(paths.url, "https://github.com/rust-lang/rust.vim.git");
        assert_eq!(paths.bare, "/data/repos/rust-lang/rust.vim.git");
        assert_eq!(paths.worktree, "/data/worktrees/rust-lang/rust.vim/7");
    }

    #[test]
    fn explains_repos_the_token_cant_read() {
        let error = remote_error("o/secret", "Couldn't clone o/secret".into())(
            "remote: Repository not found.\nfatal: repository 'https://github.com/o/secret.git/' not found".into(),
        );
        assert!(
            error
                .to_string()
                .starts_with("Git error: Can't access o/secret on GitHub."),
            "{error}"
        );

        let error = remote_error("o/r", "Couldn't fetch".into())("fatal: boom".into());
        assert_eq!(error.to_string(), "Git error: Couldn't fetch: fatal: boom");
    }

    // --- Against a local remote -----------------------------------------------

    /// Runs git in `dir` for fixture setup and returns its trimmed stdout.
    fn run(dir: &str, args: &[&str]) -> String {
        let output = std::process::Command::new(git_path())
            .args(["-C", dir, "-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "git {args:?} failed: {stderr}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// Writes `files` in the work repo `dir`, commits them and returns the commit.
    fn commit(dir: &str, files: &[(&str, &str)]) -> String {
        for (path, content) in files {
            std::fs::write(format!("{dir}/{path}"), content).unwrap();
        }
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-q", "-m", "change"]);
        run(dir, &["rev-parse", "HEAD"])
    }

    fn read(dir: &str, path: &str) -> String {
        std::fs::read_to_string(format!("{dir}/{path}")).unwrap()
    }

    #[tokio::test]
    async fn keeps_agent_config_out_of_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().to_str().unwrap();
        let remote = format!("{tmp}/remote/octo/demo.git");
        let work = format!("{tmp}/work");
        for dir in [&remote, &format!("{work}/.codex"), &format!("{work}/.claude"), &format!("{work}/src/.claude")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        run(&remote, &["init", "-q", "--bare"]);
        run(&work, &["init", "-q", "-b", "main"]);
        commit(&work, &[("a.txt", "one\n")]);
        run(&work, &["push", "-q", &remote, "HEAD:refs/heads/main"]);
        let config = [
            (".codex/config.toml", "[mcp_servers.x]\n"),
            (".claude/settings.json", "{}\n"),
            (".mcp.json", "{}\n"),
            ("src/.claude/notes.md", "kept\n"),
        ];
        commit(&work, &config);
        run(&work, &["push", "-q", &remote, "HEAD:refs/pull/1/head"]);

        // PR 2 makes them symlinks to a tracked directory holding the config.
        run(&work, &["rm", "-q", "-r", ".codex", ".claude"]);
        std::fs::create_dir_all(format!("{work}/cfg")).unwrap();
        for link in [".codex", ".claude"] {
            std::os::unix::fs::symlink("cfg", format!("{work}/{link}")).unwrap();
        }
        commit(&work, &[("cfg/config.toml", "[mcp_servers.x]\n"), ("cfg/settings.json", "{}\n")]);
        run(&work, &["push", "-q", &remote, "HEAD:refs/pull/2/head"]);

        let manager =
            CheckoutManager::with_remote_base(format!("{tmp}/data"), format!("file://{tmp}/remote/"));
        for number in [1, 2] {
            let checkout = manager.prepare("octo/demo".into(), number, "main".into(), None).await.unwrap();
            let wt = &checkout.worktree_path;
            for hidden in [".codex", ".claude", ".mcp.json"] {
                // Not following symlinks: a link that's there is a link the agent follows.
                assert!(std::fs::symlink_metadata(format!("{wt}/{hidden}")).is_err(), "#{number} {hidden}");
            }
            assert_eq!(read(wt, "a.txt"), "one\n");
            assert_eq!(read(wt, "src/.claude/notes.md"), "kept\n");
            if number == 1 {
                assert_eq!(manager.diff(checkout).await.unwrap().len(), config.len());
            }
        }
    }

    #[test]
    fn names_a_git_too_old_for_sparse_checkout() {
        assert_eq!(too_old_for_sparse("git version 2.34.1\n").as_deref(), Some("2.34.1"));
        assert_eq!(too_old_for_sparse("git version 1.9.5"), Some("1.9.5".into()));
        assert_eq!(too_old_for_sparse("git version 2.35.0"), None);
        assert_eq!(too_old_for_sparse("git version 2.50.1 (Apple Git-155)"), None);
        assert_eq!(too_old_for_sparse(""), None);
    }

    #[test]
    fn removes_agent_config_an_agent_wrote() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (wt, outside) = (tmp.path().join("wt"), tmp.path().join("outside"));
        for dir in [wt.join(".claude/hooks"), wt.join("src/.claude"), outside.clone()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(wt.join(".claude/settings.json"), "{}").unwrap();
        std::fs::write(wt.join(".mcp.json"), "{}").unwrap();
        std::fs::write(outside.join("config.toml"), "").unwrap();
        std::os::unix::fs::symlink(&outside, wt.join(".codex")).unwrap();
        let wt_str = wt.to_str().unwrap();

        remove_agent_config(wt_str).unwrap();
        for name in AGENT_CONFIG {
            assert!(std::fs::symlink_metadata(wt.join(name)).is_err(), "{name}");
        }
        // The link went, not what it pointed to; nested dirs aren't config.
        assert!(outside.join("config.toml").exists());
        assert!(wt.join("src/.claude").is_dir());
        remove_agent_config(wt_str).unwrap();

        // What can't be removed fails the start.
        std::fs::write(wt.join(".mcp.json"), "{}").unwrap();
        std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o555)).unwrap();
        let error = remove_agent_config(wt_str).unwrap_err();
        std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(error.to_string().contains("Couldn't remove `.mcp.json`"), "{error}");
    }

    #[tokio::test]
    async fn prepares_and_moves_worktrees_from_a_local_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().to_str().unwrap();
        let remote = format!("{tmp}/remote/octo/demo.git");
        let work = format!("{tmp}/work");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        run(&remote, &["init", "-q", "--bare"]);
        run(&remote, &["config", "uploadpack.allowFilter", "true"]);
        run(&work, &["init", "-q", "-b", "main"]);

        let base = commit(&work, &[("a.txt", "one\ntwo\n"), ("b.txt", "keep\n")]);
        run(&work, &["push", "-q", &remote, "HEAD:refs/heads/main"]);
        let head = commit(&work, &[("a.txt", "one\nTWO\n"), ("c.txt", "new\n")]);
        run(
            &work,
            &[
                "push",
                "-q",
                &remote,
                "HEAD:refs/pull/1/head",
                "HEAD:refs/pull/2/head",
            ],
        );
        // The base branch moves on after the PR branched off.
        run(&work, &["checkout", "-q", &base]);
        let base_tip = commit(&work, &[("b.txt", "kept\n")]);
        run(&work, &["push", "-q", &remote, "HEAD:refs/heads/main"]);

        let manager = CheckoutManager::with_remote_base(
            format!("{tmp}/data"),
            format!("file://{tmp}/remote/"),
        );
        let prepare = |number, pin: Option<&str>| {
            manager.prepare(
                "octo/demo".into(),
                number,
                "main".into(),
                pin.map(Into::into),
            )
        };
        // Two PRs of one repo at once: the lock keeps their clones and fetches apart.
        let (first, second) = tokio::join!(prepare(1, None), prepare(2, None));
        let first = first.unwrap();
        second.unwrap();
        assert_eq!(first.latest_head_oid, head);
        assert_eq!(
            first.worktree_path,
            format!("{tmp}/data/worktrees/octo/demo/1")
        );
        assert_eq!(
            (&first.head_oid, &first.base_oid, &first.merge_base_oid),
            (&head, &base_tip, &base)
        );
        assert_eq!(read(&first.worktree_path, "a.txt"), "one\nTWO\n");
        // Blobless, so the checkout above already fetched blobs lazily.
        let bare = format!("{tmp}/data/repos/octo/demo.git");
        assert_eq!(
            run(&bare, &["config", "remote.origin.partialclonefilter"]),
            "blob:none"
        );
        assert!(!Path::new(&format!("{bare}.partial")).exists());

        let files = manager.diff(first.clone()).await.unwrap();
        let summary: Vec<_> = files.iter().map(|f| (f.path.as_str(), f.status)).collect();
        // b.txt changed only on the base, so like on GitHub it isn't part of the PR.
        assert_eq!(
            summary,
            [
                ("a.txt", FileStatus::Modified),
                ("c.txt", FileStatus::Added)
            ]
        );
        assert_eq!(
            files[0].hunks[0].lines[1],
            line(Removed, Some(2), None, "two")
        );

        // Same head: the agent's edits stay.
        std::fs::write(format!("{}/a.txt", first.worktree_path), "agent edit\n").unwrap();
        std::fs::write(
            format!("{}/cache.tmp", first.worktree_path),
            "build cache\n",
        )
        .unwrap();
        let again = prepare(1, None).await.unwrap();
        assert_eq!(again, first);
        assert_eq!(read(&first.worktree_path, "a.txt"), "agent edit\n");
        // The diff is between commits, so the edit doesn't show up in it.
        assert_eq!(manager.diff(again).await.unwrap(), files);

        // A new push moves the worktree, dropping edits but not untracked files.
        run(&work, &["checkout", "-q", &head]);
        let pushed = commit(&work, &[("c.txt", "newer\n")]);
        run(
            &work,
            &["push", "-q", "-f", &remote, "HEAD:refs/pull/1/head"],
        );
        let moved = prepare(1, None).await.unwrap();
        assert_eq!(moved.head_oid, pushed);
        assert_eq!(read(&moved.worktree_path, "a.txt"), "one\nTWO\n");
        assert_eq!(read(&moved.worktree_path, "c.txt"), "newer\n");
        assert_eq!(read(&moved.worktree_path, "cache.tmp"), "build cache\n");

        // A worktree deleted by hand is recreated.
        std::fs::remove_dir_all(&moved.worktree_path).unwrap();
        let recreated = prepare(1, None).await.unwrap();
        assert_eq!(read(&recreated.worktree_path, "c.txt"), "newer\n");

        // Drafts pin the PR to `pushed`. A force-push moves `latest_head_oid` only,
        // even once the remote has dropped `pushed`.
        let pinned_files = manager.diff(moved.clone()).await.unwrap();
        run(&work, &["checkout", "-q", &head]);
        let forced = commit(&work, &[("a.txt", "forced\n")]);
        run(
            &work,
            &["push", "-q", "-f", &remote, "HEAD:refs/pull/1/head"],
        );
        run(&remote, &["gc", "-q", "--prune=now"]);
        let on_remote = std::process::Command::new(git_path())
            .args(["-C", &remote, "cat-file", "-e", &pushed])
            .status()
            .unwrap();
        assert!(
            !on_remote.success(),
            "the remote should have dropped {pushed}"
        );
        let pinned = prepare(1, Some(&pushed)).await.unwrap();
        assert_eq!(
            (&pinned.head_oid, &pinned.latest_head_oid),
            (&pushed, &forced)
        );
        assert_eq!(pinned.merge_base_oid, base);
        assert_eq!(read(&pinned.worktree_path, "c.txt"), "newer\n");
        assert_eq!(manager.diff(pinned).await.unwrap(), pinned_files);
        assert_eq!(run(&bare, &["rev-parse", "refs/ghv/pinned/1"]), pushed);

        // Unpinning moves the worktree to the latest head and drops the pin ref.
        let unpinned = prepare(1, None).await.unwrap();
        assert_eq!(
            (&unpinned.head_oid, &unpinned.latest_head_oid),
            (&forced, &forced)
        );
        assert_eq!(read(&unpinned.worktree_path, "a.txt"), "forced\n");
        assert_eq!(run(&bare, &["for-each-ref", "refs/ghv/pinned"]), "");

        // A pinned commit that was never fetched here is fetched by id.
        let orphan = commit(&work, &[("a.txt", "orphan\n")]);
        run(&work, &["push", "-q", &remote, "HEAD:refs/tmp/orphan"]);
        run(&remote, &["update-ref", "-d", "refs/tmp/orphan"]);
        let fetched = prepare(2, Some(&orphan)).await.unwrap();
        assert_eq!(
            (&fetched.head_oid, &fetched.latest_head_oid),
            (&orphan, &head)
        );
        assert_eq!(read(&fetched.worktree_path, "a.txt"), "orphan\n");

        // One the remote doesn't have either can't be shown.
        let gone = "1234567890123456789012345678901234567890";
        let message = prepare(2, Some(gone)).await.unwrap_err().to_string();
        assert!(
            message.contains("(1234567) is no longer available on GitHub"),
            "{message}"
        );
        let message = prepare(2, Some("1234567")).await.unwrap_err().to_string();
        assert!(message.contains("invalid pinned commit"), "{message}");
    }

    #[tokio::test]
    async fn reports_missing_prs() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().to_str().unwrap();
        let remote = format!("{tmp}/remote/octo/demo.git");
        std::fs::create_dir_all(&remote).unwrap();
        run(&remote, &["init", "-q", "-b", "main"]);
        commit(&remote, &[("a.txt", "a\n")]);

        let manager = CheckoutManager::with_remote_base(
            format!("{tmp}/data"),
            format!("file://{tmp}/remote/"),
        );
        let message = manager
            .prepare("octo/demo".into(), 9, "main".into(), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("Couldn't fetch PR #9 and branch main of octo/demo"),
            "{message}"
        );
        assert!(message.contains("refs/pull/9/head"), "{message}");
    }

    /// `cargo test -p ghv_core -- --ignored`
    #[tokio::test]
    #[ignore = "needs network and gh login"]
    async fn prepares_a_live_pr() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = CheckoutManager::new(tmp.path().to_str().unwrap().into());
        let checkout = manager
            .prepare("octocat/Hello-World".into(), 1, "master".into(), None)
            .await
            .unwrap();
        assert_eq!(
            checkout.head_oid,
            "7044a8a032e85b6ab611033b2ac8af7ce85805b2"
        );
        let files = manager.diff(checkout).await.unwrap();
        assert!(!files.is_empty());
        assert!(files.iter().all(|f| !f.hunks.is_empty() || f.is_binary));
    }
}

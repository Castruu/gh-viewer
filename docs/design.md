# Review window with agent support

Status: agreed 2026-11-24, being implemented on `feat/review-window`.

GH Viewer grows from a menu bar list into a local PR review app. You read the
diff in a native window. Then you select lines and ask your own coding agent
(Claude Code or Codex) about them. The agent runs in a checkout of the PR, so it
can read callers and run things. It can also draft review comments, and you
edit and submit those.

## Decisions

| # | Decision |
|---|---|
| 1 | Main window with three panes: a sidebar of PRs grouped by repo, the diff, and agent chat. The menu bar icon stays as the badge and entry point. The app is menu-bar-only until the window opens, gets a Dock icon while it's open, and drops it again on close. |
| 1a | The sidebar adds open PRs you've already reviewed (`reviewed-by:@me`), because GitHub removes your review request once you review. Repo headers show how many PRs wait on you, and those repos sort first. |
| 1b | Clicking a PR in the menu bar opens it in the review window. "Open in browser" moves to a context menu. |
| 2 | The diff is computed locally from an app-managed checkout, not from GitHub's files API. |
| 3 | Line-based unified diff only. No structural (difftastic-style) diff and no split view in v1. |
| 4 | "Viewed" per file syncs with GitHub's own state (`viewerViewedState`, `markFileAsViewed`). |
| 5 | The agent drafts comments through an MCP server that the app serves over HTTP on localhost. Drafts stay local until you submit. Every draft is checked against the diff. |
| 5a | Drafts are pinned to the commit they were written against. While a PR has drafts, its diff stays on that commit, with a "new commits pushed" banner, and the review is submitted against it. Submitting or discarding the drafts moves the diff to the latest head. |
| 5b | If you already have a pending review on github.com on the same commit, submitting adds your drafts to it and submits it. Retries skip comments that are already in it. Drafts are only added while that commit is still the PR's head, because GitHub puts added comments on the head. The app never deletes anything on GitHub. |
| 5c | Agent drafts don't need accepting. They're sent like yours, but the submit sheet lists them separately under "Drafted by Claude/Codex". |
| 5d | Comments in your github.com pending review show inline in the diff as read-only "Pending on GitHub" cards. |
| 6 | The app passes through each agent's own permission settings and adds no default of its own. Permission requests show in the chat. The one exception: calls to the app's own MCP tools (`get_pr_context`, `list_review_threads`, `draft_comment`) are allowed once automatically, because they're local and reversible. |
| 6a | Agent sessions load only your own settings, never the PR's, so opening a PR can't run its hooks or MCP servers. Worktrees are sparse checkouts without `/.claude`, `/.codex` and `/.mcp.json`, whatever their type, and those are also removed before each session starts. Claude sessions also get `settingSources: ["user"]`. The sparse checkout is needed because codex-acp trusts every session folder and claude-agent-acp reads the project's permission mode regardless. Those files still show in the diff, but agents can't read them from disk. |
| 7 | We build the chat UI (streaming text, tool calls, permission prompts) and a diff view that stays fast on large PRs. |
| 7b | Chat transcripts are saved per PR and shown again after a restart. The agent's session doesn't survive a restart, so a note marks where the new session starts. "Clear Chat" deletes a transcript. Markdown tables in replies are drawn as tables. |
| 7a | Themes: Argus's four palettes (Cool by default, Soft, Focus, Vivid) set the accent colors: statuses, the waiting dot, repo names, diff tints, selection and agent drafts. Each palette has a dark variant (Argus's colors) and a light variant. A separate Appearance setting picks System, Light or Dark. Both are chosen from the palette button in the toolbar (click for the next palette, hold to choose) and remembered. |
| 8 | The app never shows a login and never touches agent credentials. The user signs in to `claude` or `codex` themselves, and API keys work through the environment. The name doesn't use "Claude Code"; plain text like "works with Claude Code and Codex" is fine. |

## Architecture

```
 MenuBarExtra (exists) ──click PR──▶ Review window (new)
                                     ┌──────────┬────────────────┬─────────────┐
                                     │ sidebar  │ files + diff   │ agent chat  │
                                     │ by repo  │ select lines ──┼─▶ ask       │
                                     └────┬─────┴───────┬────────┴──────▲──────┘
                                          │             │               │ events (callback)
 ─────────────── UniFFI ──────────────────▼─────────────▼───────────────┼────────
 ghv_core (Rust)
   lib.rs       GitHubClient: dashboard (exists), graphql helper, gh token
   pr.rs        PR detail, review threads, viewed state, submit review, draft store
   checkout.rs  blobless clone per repo, worktree per PR, `git diff` → DiffFile[]
   agent.rs     ACP client: spawns adapters, sessions, prompts, permission round trip
   mcp.rs       localhost MCP server: get_pr_context, list_review_threads, draft_comment
                                                                        │
       spawn (stdio, JSON-RPC)  ┌───────────────────────────────────────┘
                                ▼
   npx @agentclientprotocol/claude-agent-acp  |  npx @agentclientprotocol/codex-acp
        cwd = worktree at PR head;  MCP tools over http://127.0.0.1:<port>/…
```

The Swift side stays thin: views, plus small `@Observable` models that call
the core and receive events. All GitHub, git, and agent logic lives in Rust.

### Storage

All of this lives under `~/Library/Application Support/GHViewer/`:

```
repos/<owner>/<name>.git            bare, blobless clone (--filter=blob:none)
worktrees/<owner>/<name>/<number>/  detached worktree at the PR head
drafts/<owner>/<name>/<number>.json unsent review comments (survive restarts)
drafts/<owner>/<name>/<number>.submit.json a submit that hasn't been settled
```

## Flows

**Open a PR.** `pr.rs` fetches the PR's node id, base and head refs and OIDs,
title, body, review threads, and the viewed state of each file. `checkout.rs`
clones the repo if needed. It then fetches `pull/<n>/head` and the base branch,
computes the merge base, and moves the PR's worktree to the head. Next it runs
`git diff -M <merge-base> <head>`. That's the same three-dot diff GitHub shows,
so line numbers and hunks match what GitHub accepts for comments. The diff is
between commits, not the working tree, so edits the agent makes never show up
as part of the PR.

Each review thread carries the commit its lines refer to, read from its
comments. That's usually the head, but GitHub keeps a comment posted on an
older commit there until the next push. A thread shows under a line only when
its commit is the diff's; otherwise it's listed in the file's header.

Git auth uses `gh auth git-credential` as a per-command credential helper. No
token is written to disk or into a remote URL. The app's git commands ignore
your global and system git config (`GIT_CONFIG_GLOBAL=/dev/null`). Otherwise,
URL rewrites to ssh, global hooks, LFS filters, and `diff.*` settings would
break the checkouts or change the diff. Proxies set through `HTTPS_PROXY`
still work. Proxy or CA settings in `~/.gitconfig` don't.

**Ask about a selection.** You select a line range in one file. The app sends
a prompt to the agent session for that PR. The prompt is a context packet:
repo, PR number, title and body, base and head OIDs, path, line range and side,
the selected lines, and existing threads on those lines, followed by your
question. It doesn't send file contents, because the agent can read the
worktree itself. Streamed updates such as text, tool calls, and plans come back
through a UniFFI callback interface.

**Agent drafts a comment.** Each session is created with the app's MCP server
(`type: http`, a per-session URL plus a bearer token). Both adapters advertise
HTTP MCP support. `draft_comment(path, line, side, start_line?, start_side?,
body)` checks that the target lines are in the diff, the same rule GitHub
enforces. If not, it returns an error the agent can act on. Otherwise it adds
the comment to the PR's local drafts, and the UI shows it right away. The agent
can never post to GitHub.

**Submit.** GitHub is the source of truth, and the app never deletes anything
there. Only the drafts the submit sheet showed go out, in two steps, so a
retry after a lost answer never posts a review twice. The gh token is read
once and used for every call of the submit, so switching accounts midway can't
split it:

1. The drafts go into a pending review on their pinned commit. If you already
   have one on that commit, the drafts it lacks are added with
   `addPullRequestReviewThread`. That call takes no commit: GitHub puts the
   comments on the PR's head, whatever the review's commit. So if the PR has
   newer commits, the submit stops and asks you to submit or delete that review
   on github.com. If yours is on another commit, the submit stops too.
   Otherwise `addPullRequestReview` without an event starts one with every
   draft as a `DraftPullRequestReviewThread`.
2. A submit record (the PR's node id, the signed-in account, the review's id
   and link, the commit, event, body and the drafts as sent) is saved next to
   the drafts. The file, its directory and each directory level down from the
   app's data dir are synced. Then `submitPullRequestReview` submits the
   review with the event and body. If GitHub refuses it, e.g. an approval of
   your own PR, nothing is posted and the pending review stays: change the
   verdict or fix the cause and submit again. Any other error before GitHub
   confirms keeps the record, and retrying is safe.

Drafts and review comments are paired one to one, on path, original range
(which later pushes don't move; the current one if GitHub leaves it out), text
and side where it's known, against all of a review's comments. GitHub doesn't
report a comment's side, so it's read from the diff hunk: an added line's is
right, a removed line's left, and a context line's could be either. Comments
on a known side are paired first. Once GitHub confirms, the review is read
back and only the drafts it holds are deleted, then the record. Drafts added
meanwhile stay, and so do any it lacks, e.g. deleted from the pending review
on github.com, with a warning. If that read fails, the drafts and the record
stay, and the warning says to submit again to finish, which won't post twice.
If the cleanup fails, the submit still succeeds. Its warning says to submit
again if the record is still there, and not to if only saving its removal
failed.

A later submit with a record settles it first. Signed in as another account,
it stops. Otherwise it looks that review up. If GitHub leaves out the review's
commit, it's read again once; if the commit is still missing, the submit fails
and retrying is safe. Then:

- Not found: it was deleted while pending, so the record is forgotten.
- Pending on the same commit: its missing drafts are added, as in step 1, and
  it's submitted.
- Pending on another commit: its link is shown, and it's left for you.
- Submitted: the drafts GitHub's review holds are deleted here either way. If
  it's yours, and its commit, verdict and body and the drafts match this
  submit exactly, the submit is done. Otherwise the error names what was
  posted and the drafts that stayed, and the record is settled.
- Any other lookup error keeps the record and fails; retrying is safe.

Drafts in an unsettled pending review can't be edited or deleted here. Discard
Drafts waits for a running submit, deletes the drafts and then the record, and
says whether a review is still pending on github.com, or may be when GitHub
couldn't be checked, with its link. Submits and discards for one PR run one at
a time.

**Permissions.** ACP `session/request_permission` shows up as a prompt in the
chat with the agent's own options. The app answers only what you pick, or
cancels if the session ends. The exception is the app's own three MCP tools,
which are answered with `allow_once`, never `allow_always`, so nothing is
written to the agent's config.

## Agents

- Claude: `npx -y @agentclientprotocol/claude-agent-acp`
- Codex: `npx -y @agentclientprotocol/codex-acp`

Apps launched from Finder get a minimal PATH, so the core reads the user's
login shell environment (`$SHELL -l -c 'env -0'`), keeping the first read that works. It uses that
environment for every agent process, which lets `npx`, `node`, `cargo` and
friends resolve as they do in a terminal. One adapter process runs per agent
kind, with one ACP session per PR. The app tells the agent it offers no
file-system or terminal capabilities, so agents use their built-in tools inside
the worktree.

The worktree holds the PR author's code, and agents read config from their cwd.
So worktrees are sparse checkouts without `/.claude`, `/.codex` and
`/.mcp.json` (no trailing slash, so a symlink or file is excluded too). A
worktree is added with no checkout, gets the patterns, and only then checks out
the head, so those paths never exist on disk. They're also removed, without
following symlinks, right before each session starts, in case an agent wrote
them. Claude sessions load only the user's own settings
(`settingSources: ["user"]`). Otherwise a PR's hooks and MCP servers would run
on the reviewer's machine; codex-acp trusts every session root, so Codex would
load the PR's `.codex/config.toml`. The sparse checkout needs git 2.35; the
core uses the first git it finds that's new enough.

Stop sends `session/cancel` for that question's turn only. If the agent hasn't
ended the turn 5 s later, the core closes the session (`session/close`, MCP
endpoint revoked) and drops whatever it sends later; the next question opens a
new session. Agent sessions stay open for the 5 most recently used PRs.

## Not in v1

- Structural diff and split view (decision 3).
- Syntax highlighting. A likely follow-up is `syntect` in the core, which emits
  spans per line.
- "Changes since my last review" (`git diff <last-reviewed-head> <head>`).
- Cleaning up worktrees of closed PRs. For now, delete
  `~/Library/Application Support/GHViewer/worktrees` by hand.
- Configurable agent commands. The two above are hard-coded.
- Replying to or resolving existing threads from the app.

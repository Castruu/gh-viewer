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
| 5b | If you already have a pending review on github.com, submitting adds your drafts to it and submits it. Retries skip comments that are already in it. |
| 5c | Agent drafts don't need accepting. They're sent like yours, but the submit sheet lists them separately under "Drafted by Claude/Codex". |
| 5d | Comments in your github.com pending review show inline in the diff as read-only "Pending on GitHub" cards. |
| 6 | The app passes through each agent's own permission settings and adds no default of its own. Permission requests show in the chat. The one exception: calls to the app's own MCP tools (`get_pr_context`, `list_review_threads`, `draft_comment`) are allowed once automatically, because they're local and reversible. |
| 6a | Agent sessions load only your own settings, never the PR's. A PR's `.claude/settings.json`, `.mcp.json` and CLAUDE.md are ignored (`settingSources: ["user"]`), so opening a PR can't run its hooks. The agent can still read those files as plain files. |
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

**Submit.** One `addPullRequestReview` mutation carries the event (approve,
request changes, or comment), the body, `commitOID` set to the drafts' pinned
commit, and every draft as a `DraftPullRequestReviewThread`. If you already
have a pending review on GitHub, each draft is added to it with
`addPullRequestReviewThread`, skipping any already there, and then
`submitPullRequestReview` submits it. Drafts are deleted only after GitHub
accepts the review.

**Permissions.** ACP `session/request_permission` shows up as a prompt in the
chat with the agent's own options. The app answers only what you pick, or
cancels if the session ends. The exception is the app's own three MCP tools,
which are answered with `allow_once`, never `allow_always`, so nothing is
written to the agent's config.

## Agents

- Claude: `npx -y @agentclientprotocol/claude-agent-acp`
- Codex: `npx -y @agentclientprotocol/codex-acp`

Apps launched from Finder get a minimal PATH, so the core reads the user's
login shell environment once (`$SHELL -l -c 'env -0'`). It uses that
environment for every agent process, which lets `npx`, `node`, `cargo` and
friends resolve as they do in a terminal. One adapter process runs per agent
kind, with one ACP session per PR. The app tells the agent it offers no
file-system or terminal capabilities, so agents use their built-in tools inside
the worktree.

## Not in v1

- Structural diff and split view (decision 3).
- Syntax highlighting. A likely follow-up is `syntect` in the core, which emits
  spans per line.
- "Changes since my last review" (`git diff <last-reviewed-head> <head>`).
- Cleaning up worktrees of closed PRs. For now, delete
  `~/Library/Application Support/GHViewer/worktrees` by hand.
- Configurable agent commands. The two above are hard-coded.
- Replying to or resolving existing threads from the app.

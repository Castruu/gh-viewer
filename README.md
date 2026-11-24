# GH Viewer

A macOS app for reviewing pull requests on github.com, with your own coding
agent beside the diff.

- **Menu bar**: the PRs waiting on your review (the icon shows how many) and
  your own open PRs, with review decision and CI status. Click a PR to open it
  in the review window. Right-click to open it in your browser.
- **Review window**: your PRs grouped by repo, including ones you've already
  reviewed. Each PR shows a unified diff computed from a local checkout.
  "Viewed" checkboxes sync with GitHub.
- **Comments**: select lines to draft review comments, then submit them as one
  review (comment, approve or request changes). Drafts are saved locally until
  you submit.
- **Agent chat**: ask Claude Code or Codex about the selected lines. The agent
  runs in a checkout of the PR, so it can read callers and run things. Ask it to
  review the changes and it drafts comments for you to edit or discard. Nothing
  is posted until you submit.
- Refreshes every 2 minutes, and whenever you open the menu if the data is
  more than 30 seconds old. ⌘R refreshes manually.

| Icon | Meaning |
|---|---|
| green check | Approved |
| red cross | Changes requested |
| orange clock | Review required |
| dashed circle | No review decision yet |
| green / red / yellow dot | Checks passed / failed / running |

## Requirements

- macOS 14 or later
- [GitHub CLI](https://cli.github.com), signed in: `brew install gh && gh auth login`.
  The app reads the token from `gh auth token` on each refresh and never stores it.
- Rust (stable) and the Xcode Command Line Tools (`xcode-select --install`) to build.
  Full Xcode is not required.
- For the agent chat: Node.js (`npx`) plus [Claude Code](https://code.claude.com)
  and/or [Codex](https://github.com/openai/codex), installed and signed in
  yourself. GH Viewer starts them through their
  [ACP](https://agentclientprotocol.com) adapters
  (`@agentclientprotocol/claude-agent-acp`, `@agentclientprotocol/codex-acp`).
  It never handles agent credentials, and each agent's own permission settings
  apply. GH Viewer's own review tools are the one exception: they're allowed
  automatically.

GH Viewer keeps its clones, per-PR worktrees and drafts in
`~/Library/Application Support/GHViewer`. To reclaim disk space, delete the
`repos` and `worktrees` folders there while the app isn't running.

## Build and run

```sh
scripts/build.sh          # builds build/GHViewer.app
scripts/build.sh --open   # builds, quits any running copy and launches it
```

To keep it around, copy `build/GHViewer.app` to `/Applications`. The build is
ad-hoc signed, which is fine for running on the machine that built it.

For development, `open build/GHViewer.app --args --open-review-window [<PR URL>]`
opens the review window at launch, optionally with a PR from your lists selected.

## Architecture

```
app/    SwiftUI app (menu bar + review window). Thin: UI and state only.
core/   Rust library, exported via UniFFI:
          lib.rs       GitHub GraphQL client, dashboard
          pr.rs        PR detail, threads, viewed state, drafts, review submission
          checkout.rs  blobless clones, a worktree per PR, local diff
          agent.rs     ACP client for Claude Code and Codex
          mcp.rs       localhost MCP server with the agents' review tools
          review.rs    ties them together for the window
docs/design.md  decisions and data flows for the review window
uniffi-bindgen/  Generates the Swift bindings for core.
scripts/build.sh  cargo build -> generate bindings -> swift build -> .app bundle
```

The build script generates `app/Sources/ghv_coreFFI`, `app/Sources/GHViewer/Generated`
and `app/RustLib`. They're git-ignored, so run the script before `swift build`
or opening `app/` in an editor.

## Tests

```sh
cargo test -p ghv_core                    # unit and local integration tests
cargo test -p ghv_core -- --ignored       # live: GitHub (read-only) and your Claude/Codex logins
```

## License

MIT. See [LICENSE](LICENSE).

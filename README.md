<p align="center">
  <img src="docs/assets/icon.svg" width="128" height="128" alt="GH Viewer icon">
</p>

<h1 align="center">GH Viewer</h1>

<p align="center">
  <strong>Review GitHub pull requests in a native Mac app, with your coding agent beside the diff.</strong>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/macOS-14%2B-000000?logo=apple&logoColor=white" alt="macOS 14+">
  <img src="https://img.shields.io/badge/Rust%20%2B%20SwiftUI-orange" alt="Rust + SwiftUI">
  <img src="https://img.shields.io/badge/agents-Claude%20Code%20%7C%20Codex-6E56CF" alt="Agents: Claude Code, Codex">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT license"></a>
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#features">Features</a> ·
  <a href="#privacy-and-safety">Privacy</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#development">Development</a>
</p>

<p align="center">
  <img src="docs/assets/screenshots/agent-chat.png" alt="GH Viewer: a pull request's diff with Claude answering a question about the selected lines">
</p>

GH Viewer lives in your menu bar and shows the pull requests waiting on you.
Open one and you get a fast, native diff from a local checkout, with Claude Code
or Codex next to it. Select lines and ask about them: the agent runs in the PR's
checkout, so it can read the callers and run the tests. Ask it to review the
changes and it drafts comments for you to edit or throw away. Nothing reaches
GitHub until you submit.

## Quick start

You need macOS 14 or later and the [GitHub CLI](https://cli.github.com), signed in.

```sh
brew install gh && gh auth login       # once
xcode-select --install                 # once, if you don't have the Command Line Tools
curl https://sh.rustup.rs -sSf | sh    # once, if you don't have Rust

git clone https://github.com/Castruu/gh-viewer.git
cd gh-viewer
scripts/build.sh --open
```

The icon appears in your menu bar with the number of PRs waiting on your review.
Click a PR to open it, or press **⌘O** in the menu for the review window.
To keep the app, copy `build/GHViewer.app` to `/Applications`.

For the agent chat, also install [Node.js](https://nodejs.org) and sign in to
[Claude Code](https://code.claude.com) and/or [Codex](https://github.com/openai/codex).
There's nothing to configure in GH Viewer.

## Features

### Your review queue, one click away

<img src="docs/assets/screenshots/menu-bar.png" width="380" align="right" alt="The menu bar list: PRs waiting on your review and your own PRs, with review and CI status">

The menu bar shows the PRs that wait on your review (the icon shows how many)
and your own open PRs, with their review decision and CI status. Click one to
review it, right-click to open it in your browser.

It refreshes every 2 minutes, and when you open the menu if the data is more
than 30 seconds old. **⌘R** refreshes now.

Each PR says its state in words: Draft, Approved, Changes requested, Checks failing
or Checks running. A PR with nothing to report shows nothing.

Colors follow the theme you pick in Settings (**⌘,** with the review window open):
Cool, Soft, Focus or Vivid, in light or dark. Settings also has four app icons to pick from
for the Dock.

<br clear="right">

### A diff that keeps up with you

<img src="docs/assets/screenshots/review-window.png" alt="The review window: PRs grouped by repo, the changed files and the diff">

- Your PRs grouped by what they need: your review, your own, and the ones you've already reviewed.
- The window opens on the first PR waiting on you, and moves to the next one after you submit a review.
- The sidebar switches between your pull requests and the open one's files, as a folder tree. Hover a diff line for the "+", click it to comment or ask the agent.
- A unified diff computed from a local checkout, not fetched from GitHub's API.
- "Viewed" checkboxes sync with GitHub. Viewed and very large files start collapsed.
- Review threads from GitHub, placed on the lines they're about.

### An agent that has read the code

- Select lines and ask Claude Code or Codex about them, or start from
  _Summarize this PR_, _What could break?_ or _Review the changes and draft
  comments_.
- The agent works in a checkout of the PR, with your own login and permission
  settings. It can open other files, follow callers and run commands.
- Switch between Claude and Codex per question.

### Comments you control

<img src="docs/assets/screenshots/drafts.png" alt="A draft review comment written by the agent, shown inline in the diff">

<img src="docs/assets/screenshots/submit.png" width="420" align="right" alt="The Submit Review sheet, listing the draft Claude wrote">

- Select lines to draft a comment yourself, or let the agent draft them.
- Drafts are saved locally until you submit. Edit or delete any of them.
- Submit them as one review: comment, approve or request changes.
- Agent drafts are labeled as such, here and in the diff, so you check them
  before they go out.

<br clear="right">

## Privacy and safety

- **No token storage.** GH Viewer reads your token from `gh auth token` on each
  refresh and never stores it.
- **No agent credentials.** Claude Code and Codex run with your own logins,
  through their [ACP](https://agentclientprotocol.com) adapters
  (`@agentclientprotocol/claude-agent-acp`, `@agentclientprotocol/codex-acp`).
  Each agent's own permission settings apply. GH Viewer's review tools are the
  one exception, allowed automatically: they read the PR and its review threads,
  and write drafts on your Mac.
- **Nothing is posted until you submit.** Agent drafts stay on your Mac.
- **Your data stays local.** Clones, per-PR worktrees, drafts and chats live in
  `~/Library/Application Support/GHViewer`. To reclaim disk space, delete the
  `repos` and `worktrees` folders there while the app isn't running.

## How it works

```
             menu bar + review window (SwiftUI)
                            │  UniFFI
                            ▼
                    ghv_core (Rust)
     ┌──────────────┬───────┴──────┬─────────────────┐
     ▼              ▼              ▼                 ▼
 GitHub GraphQL   git clone +    ACP client ──▶  Claude Code / Codex
 (token from gh)  worktree       (agent.rs)          │
 lib.rs, pr.rs    per PR                             │ review tools
                  checkout.rs    MCP server ◀────────┘
                                 on localhost (mcp.rs)
```

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

## FAQ

**macOS asks GH Viewer for access to my Documents folder.**
Agents run as part of GH Viewer, so macOS asks GH Viewer when your agent or one
of its hooks reads a protected folder such as `~/Documents`. The agent waits
until you answer. The build is ad-hoc signed and that signature changes with
every build, so each new build asks again.

**Can I use it on a machine other than the one that built it?**
The ad-hoc signature is fine for running on the machine that built it. There
are no signed releases yet.

**Does it work with GitHub Enterprise?**
Not yet: it talks to github.com.

## Development

Requirements: Rust (stable) and the Xcode Command Line Tools
(`xcode-select --install`). Full Xcode is not required.

```sh
scripts/build.sh          # builds build/GHViewer.app
scripts/build.sh --open   # builds, quits any running copy and launches it
```

The build script generates `app/Sources/ghv_coreFFI`, `app/Sources/GHViewer/Generated`
and `app/RustLib`. They're git-ignored, so run the script before `swift build`
or opening `app/` in an editor.

`open build/GHViewer.app --args --open-review-window [<PR URL>]` opens the
review window at launch, optionally with a PR from your lists selected.

The app icon's source is `docs/assets/icon.svg`; `app/AppIcon.icns` is rendered
from it.

### Tests

```sh
cargo test -p ghv_core                    # unit and local integration tests
cargo test -p ghv_core -- --ignored       # live: GitHub (read-only) and your Claude/Codex logins
swift test --package-path app             # app logic; run scripts/build.sh first
```

The live submit tests post real reviews and push commits, so they only run on
a PR you name, one of yours that's safe to review:
`GHV_LIVE_SUBMIT_PR=owner/repo#n cargo test -p ghv_core live_submit -- --ignored --test-threads=1`.
See `mod live_submit` in `core/src/review.rs` for what it needs.

With only the Command Line Tools installed (no Xcode), swift-testing sits outside
SwiftPM's search paths, so pass them to `swift test`:

```sh
T=/Library/Developer/CommandLineTools/Library/Developer
swift test --package-path app -Xswiftc -F$T/Frameworks \
    -Xlinker -rpath -Xlinker $T/Frameworks -Xlinker -rpath -Xlinker $T/usr/lib
```

## License

MIT. See [LICENSE](LICENSE).

# GitHub Viewer

A macOS menu bar app that shows the pull requests waiting on your review and
the status of your own open pull requests on github.com.

- **Review requested**: open PRs where your review (or your team's) is requested.
  The menu bar icon shows how many there are.
- **My pull requests**: your open PRs, with review decision and CI status.
- Click a PR to open it in your browser.
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

## Build and run

```sh
scripts/build.sh          # builds build/GitHubViewer.app
scripts/build.sh --open   # builds, quits any running copy and launches it
```

To keep it around, copy `build/GitHubViewer.app` to `/Applications`. The build is
ad-hoc signed, which is fine for running on the machine that built it.

## Architecture

```
app/    SwiftUI app (MenuBarExtra). Thin: UI and polling only.
core/   Rust library: GitHub GraphQL client and data model, exported via UniFFI.
uniffi-bindgen/  Generates the Swift bindings for core.
scripts/build.sh  cargo build -> generate bindings -> swift build -> .app bundle
```

The build script generates `app/Sources/ghv_coreFFI`, `app/Sources/GitHubViewer/Generated`
and `app/RustLib`. They're git-ignored, so run the script before `swift build`
or opening `app/` in an editor.

## Tests

```sh
cargo test -p ghv_core                    # parsing and status mapping
cargo test -p ghv_core -- --ignored       # live call to GitHub with your gh login
```

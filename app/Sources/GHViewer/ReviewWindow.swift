import AppKit
import SwiftUI

/// The main window: your PRs by repo in the sidebar, the selected PR in the detail,
/// and the agent chat in an inspector.
struct ReviewWindow: View {
    static let id = "review"
    /// Set by `OpenWindowAction.reviewWindow()`. Until then, a window SwiftUI opens by itself
    /// (at launch on some macOS versions, or through state restoration) closes again.
    static var isRequested = false

    let model: AppModel
    let reviews: ReviewStore
    @State private var isChatPresented = false
    @Environment(\.dismissWindow) private var dismissWindow

    var body: some View {
        NavigationSplitView {
            ReviewSidebar(model: model)
                .navigationSplitViewColumnWidth(min: 240, ideal: 300, max: 420)
        } detail: {
            PRDetail(model: model, reviews: reviews)
        }
        .inspector(isPresented: $isChatPresented) {
            ChatPane(review: model.selectedPullRequest.map(reviews.review(for:)))
                .inspectorColumnWidth(min: 260, ideal: 320, max: 480)
        }
        .toolbar {
            ToolbarItem {
                ThemeMenu()
            }
            ToolbarItem {
                RefreshButton(model: model)
            }
            ToolbarItem {
                Button {
                    isChatPresented.toggle()
                } label: {
                    Label("Agent Chat", systemImage: "bubble.left.and.bubble.right")
                }
                .help(isChatPresented ? "Hide agent chat" : "Show agent chat")
            }
        }
        .onChange(of: model.selectedPullRequest.flatMap { reviews.review(for: $0).chatFocusRequest }) { _, request in
            if request != nil {
                isChatPresented = true
            }
        }
        .frame(minWidth: 760, minHeight: 440)
        .themed()
        // The app is menu bar only, except while this window is open: then it gets a Dock icon and ⌘-Tab.
        .onAppear {
            // `.defaultLaunchBehavior(.suppressed)` needs macOS 15, and a SceneBuilder has no
            // `else` branch to apply it only there, so this guard covers every version.
            guard Self.isRequested else {
                dismissWindow(id: Self.id)
                return
            }
            NSApp.setActivationPolicy(.regular)
            NSApp.activate()
        }
        .onDisappear {
            NSApp.setActivationPolicy(.accessory)
        }
    }
}

extension OpenWindowAction {
    /// Opens the review window, or brings it to the front if it's already open.
    @MainActor
    func reviewWindow() {
        ReviewWindow.isRequested = true
        self(id: ReviewWindow.id)
        NSApp.activate()
    }
}

/// The selected PR: its header, then the diff.
private struct PRDetail: View {
    let model: AppModel
    let reviews: ReviewStore

    var body: some View {
        if let pullRequest = model.selectedPullRequest {
            VStack(spacing: 0) {
                PRHeader(pullRequest: pullRequest, isInDashboard: model.isSelectionInDashboard)
                Divider()
                DiffPane(review: reviews.review(for: pullRequest))
                    // One diff view per PR, so scroll position and selection don't leak between PRs.
                    .id(pullRequest.url)
            }
        } else {
            ContentUnavailableView("No Pull Request Selected", systemImage: "arrow.triangle.pull",
                                   description: Text("Select a pull request in the sidebar."))
        }
    }
}

private struct PRHeader: View {
    let pullRequest: PullRequest
    let isInDashboard: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                Text(pullRequest.title)
                    .font(.title2.weight(.semibold))
                    .textSelection(.enabled)
                Spacer(minLength: 0)
                Button("Open in Browser") {
                    if let url = URL(string: pullRequest.url) {
                        NSWorkspace.shared.open(url)
                    }
                }
                .help("Open this pull request on GitHub")
            }

            HStack(alignment: .firstTextBaseline, spacing: 6) {
                ReviewIcon(decision: pullRequest.reviewDecision)
                Text(byline)
                    .textSelection(.enabled)
                if pullRequest.isDraft {
                    DraftBadge()
                }
                CheckIcon(state: pullRequest.checkState)
            }
            .foregroundStyle(.secondary)

            if !isInDashboard {
                Label("No longer in your lists, so this may be out of date. It may have been merged or closed.",
                      systemImage: "info.circle")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var byline: String {
        var parts = ["\(pullRequest.repo) #\(pullRequest.number)"]
        if let author = pullRequest.author {
            parts.append(author)
        }
        return parts.joined(separator: " · ")
    }
}

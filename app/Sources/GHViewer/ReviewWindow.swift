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
    /// Open from the start: the agent is the point of the app. Closing it is remembered.
    @AppStorage("showsChat") private var showsChat = true
    @State private var isChatPresented = false
    @Environment(\.dismissWindow) private var dismissWindow

    var body: some View {
        NavigationSplitView {
            ReviewSidebar(model: model, reviews: reviews)
                .navigationSplitViewColumnWidth(min: 220, ideal: 270, max: 420)
        } detail: {
            PRDetail(model: model, reviews: reviews)
                // On the detail column, not the split view: there it becomes a third column.
                // Outside, the split view's content overflowed the window at its default size.
                .inspector(isPresented: $isChatPresented) {
                    ChatPane(review: model.selectedPullRequest.map(reviews.review(for:)))
                        .inspectorColumnWidth(min: 260, ideal: 320, max: 480)
                }
        }
        .toolbar {
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
        // An inspector that's already open in the window's first layout pushes the columns
        // past the window's edges, so it opens right after.
        .task { isChatPresented = showsChat }
        .onChange(of: isChatPresented) { showsChat = isChatPresented }
        // On opening, and again once the first dashboard arrives.
        .onChange(of: model.dashboard == nil, initial: true) { model.selectDefaultPullRequest() }
        .frame(minWidth: 760, minHeight: 440)
        .themed()
        .environment(\.openURL, OpenURLAction { $0.isWebLink ? .systemAction : .discarded })
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

extension URL {
    /// Agent replies and comments are other people's markdown: their links may open web pages,
    /// never local files or other apps.
    var isWebLink: Bool {
        ["http", "https"].contains(scheme?.lowercased() ?? "")
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
    /// The last submitted review's URL, shown briefly as a confirmation. Kept here, above the
    /// per-PR diff, so it's still on screen after moving on to the next PR.
    @State private var submittedURL: String?

    var body: some View {
        if let pullRequest = model.selectedPullRequest {
            VStack(spacing: 0) {
                PRHeader(pullRequest: pullRequest, isInDashboard: model.isSelectionInDashboard)
                Divider()
                DiffPane(review: reviews.review(for: pullRequest)) { url in
                    submittedURL = url
                    model.selectNextPullRequestToReview()
                }
                // One diff view per PR, so scroll position and selection don't leak between PRs.
                .id(pullRequest.url)
            }
            .overlay(alignment: .bottom) {
                if let submittedURL {
                    SubmitConfirmation(url: submittedURL) { self.submittedURL = nil }
                        .task(id: submittedURL) {
                            try? await Task.sleep(for: .seconds(8))
                            self.submittedURL = nil
                        }
                }
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
                    .font(.title3.weight(.semibold))
                    .textSelection(.enabled)
                Spacer(minLength: 0)
                Button("Open in Browser", systemImage: "arrow.up.right") {
                    if let url = URL(string: pullRequest.url) {
                        NSWorkspace.shared.open(url)
                    }
                }
                .buttonStyle(.borderless)
                .foregroundStyle(.secondary)
                .help("Open this pull request on GitHub")
            }

            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(byline)
                    .textSelection(.enabled)
                    .foregroundStyle(.secondary)
                PRStatusText(pullRequest: pullRequest)
            }

            if !isInDashboard {
                Label("No longer in your lists, so this may be out of date. It may have been merged or closed.",
                      systemImage: "info.circle")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 14)
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

import Foundation
import Observation

/// Owns the Rust review hub and one `PRReview` per PR opened in the window.
@MainActor
@Observable
final class ReviewStore {
    private(set) var reviews: [String: PRReview] = [:]

    @ObservationIgnored let github = GitHubClient()
    @ObservationIgnored let drafts: DraftStore
    @ObservationIgnored let hub: ReviewHub
    @ObservationIgnored private let listener = CoreListener()

    init() {
        let root = Self.dataDirectory().path
        drafts = DraftStore(root: root)
        hub = ReviewHub(root: root, github: github, drafts: drafts, listener: listener)
        drafts.setListener(listener: listener)
        listener.store = self
    }

    /// The review for a PR, created on first use. Keyed by URL like the sidebar.
    func review(for pullRequest: PullRequest) -> PRReview {
        if let review = reviews[pullRequest.url] {
            return review
        }
        let review = PRReview(pullRequest: pullRequest, store: self)
        reviews[pullRequest.url] = review
        return review
    }

    fileprivate func agentEvent(sessionId: String, event: AgentEvent) {
        reviews.values.first { $0.chat.ownsSession(sessionId) }?.apply(event, sessionId: sessionId)
    }

    fileprivate func draftsChanged(repo: String, number: UInt64) {
        // The store matches repos case-insensitively, since its files live on a case-insensitive disk.
        reviews.values
            .first { $0.number == number && $0.repo.caseInsensitiveCompare(repo) == .orderedSame }?
            .reloadDrafts()
    }

    /// ~/Library/Application Support/GHViewer: clones, worktrees and drafts.
    private static func dataDirectory() -> URL {
        URL.applicationSupportDirectory.appending(path: "GHViewer", directoryHint: .isDirectory)
    }
}

/// Receives callbacks from Rust on background threads and hands them to the main actor.
private final class CoreListener: AgentListener, DraftListener, @unchecked Sendable {
    /// Set once on the main actor right after init, and only read there.
    weak var store: ReviewStore?

    func onEvent(sessionId: String, event: AgentEvent) {
        Task { @MainActor in self.store?.agentEvent(sessionId: sessionId, event: event) }
    }

    func draftsChanged(repo: String, number: UInt64) {
        Task { @MainActor in self.store?.draftsChanged(repo: repo, number: number) }
    }
}

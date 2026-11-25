import AppKit

/// Owns the Rust review hub and one `PRReview` per PR opened in the window.
/// Not observable itself: views observe the `PRReview`s, and this is only their cache,
/// so creating one while a view body runs doesn't count as a state change.
@MainActor
final class ReviewStore {
    private(set) var reviews: [String: PRReview] = [:]

    let github = GitHubClient()
    let drafts: DraftStore
    let chats: ChatArchive
    let hub: ReviewHub
    private let listener = CoreListener()

    init() {
        let root = Self.dataDirectory().path
        drafts = DraftStore(root: root)
        chats = ChatArchive(root: Self.dataDirectory().appending(path: "chats", directoryHint: .isDirectory))
        hub = ReviewHub(root: root, github: github, drafts: drafts, listener: listener)
        drafts.setListener(listener: listener)
        listener.store = self
        // Chats are saved when a turn ends; a turn still running at quit would otherwise be lost.
        NotificationCenter.default.addObserver(forName: NSApplication.willTerminateNotification,
                                               object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.reviews.values.forEach { $0.saveChatIfRunning() }
            }
        }
    }

    /// PRs whose agent sessions stay open. Each Claude session is a CLI process.
    static let maxPRsWithAgents = 5
    private var agentPRs = RecentlyUsed()

    /// Called when `review` uses an agent. Closes the sessions of the least
    /// recently used PRs beyond `maxPRsWithAgents`, unless a turn runs there.
    func usedAgent(on review: PRReview) {
        agentPRs.use(review.url)
        trimAgentSessions()
    }

    /// Closes sessions beyond the limit that a running turn kept open, e.g.
    /// once that turn ends. Doesn't change which PRs count as recently used.
    func trimAgentSessions() {
        let evicted = agentPRs.trim(limit: Self.maxPRsWithAgents) { url in
            reviews[url]?.chat.isRunning != true
        }
        evicted.forEach { reviews[$0]?.closeAgentSessions() }
    }

    /// Called when `review` closed its last session, e.g. by Clear Chat.
    func closedAgents(on review: PRReview) {
        agentPRs.remove(review.url)
    }

    /// PRs whose diff stays in memory. A big diff is tens of megabytes, and a day of reviewing opens many.
    static let maxLoadedPRs = 6
    private var loadedPRs = RecentlyUsed()

    /// Called when `review` is shown or fetched ahead. Unloads the least recently used beyond `maxLoadedPRs`.
    func used(_ review: PRReview) {
        loadedPRs.use(review.url)
        loadedPRs.trim(limit: Self.maxLoadedPRs) { _ in true }.forEach { reviews[$0]?.unloadIfIdle() }
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

    /// ~/Library/Application Support/GHViewer: clones, worktrees, drafts and chats.
    private static func dataDirectory() -> URL {
        URL.applicationSupportDirectory.appending(path: "GHViewer", directoryHint: .isDirectory)
    }
}

/// Keys in order of use, least recent first.
struct RecentlyUsed {
    private(set) var keys: [String] = []

    /// Moves `key` to the end, as the most recently used.
    mutating func use(_ key: String) {
        remove(key)
        keys.append(key)
    }

    mutating func remove(_ key: String) {
        keys.removeAll { $0 == key }
    }

    /// Drops keys from the front while there are more than `limit`, skipping
    /// those `canDrop` refuses and the most recent, and returns them.
    mutating func trim(limit: Int, canDrop: (String) -> Bool) -> [String] {
        var dropped: [String] = []
        for old in keys.dropLast() where keys.count - dropped.count > limit && canDrop(old) {
            dropped.append(old)
        }
        keys.removeAll { dropped.contains($0) }
        return dropped
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

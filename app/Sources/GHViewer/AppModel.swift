import Foundation
import Observation

/// Holds the latest dashboard and refreshes it from the Rust core on a timer.
@MainActor
@Observable
final class AppModel {
    static let refreshInterval: Duration = .seconds(120)
    /// Retry sooner after a failure, e.g. when the Mac wakes before Wi-Fi is back.
    static let retryInterval: Duration = .seconds(15)

    private(set) var dashboard: Dashboard?
    /// Set when the last refresh failed. Any previous dashboard is kept so the lists stay usable.
    private(set) var errorMessage: String?
    private(set) var isRefreshing = false
    private(set) var lastUpdated: Date?

    /// The PR shown in the review window. The menu bar sets it too.
    var selectedPullRequestURL: String?
    /// Every PR seen since launch, so the review window can keep showing one that left the dashboard.
    private var lastKnownPullRequests: [String: PullRequest] = [:]

    private let client = GitHubClient()
    private var pollTask: Task<Void, Never>?

    init() {
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                await self.refresh()
                let delay = self.errorMessage == nil ? Self.refreshInterval : Self.retryInterval
                try? await Task.sleep(for: delay)
            }
        }
    }

    /// Called when the menu opens, so the lists aren't up to two minutes old.
    func refreshIfStale() async {
        if let lastUpdated, Date().timeIntervalSince(lastUpdated) < 30 {
            return
        }
        await refresh()
    }

    func refresh() async {
        guard !isRefreshing else { return }
        isRefreshing = true
        defer { isRefreshing = false }

        do {
            let dashboard = try await client.fetchDashboard()
            for pullRequest in dashboard.allPullRequests {
                lastKnownPullRequests[pullRequest.url] = pullRequest
            }
            self.dashboard = dashboard
            errorMessage = nil
            lastUpdated = Date()
        } catch let error as CoreError {
            errorMessage = error.message
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// The selected PR as last seen, even if a refresh has since dropped it from the dashboard.
    var selectedPullRequest: PullRequest? {
        selectedPullRequestURL.flatMap { lastKnownPullRequests[$0] }
    }

    /// False when the selected PR is no longer in any of the dashboard's lists, e.g. merged or closed.
    var isSelectionInDashboard: Bool {
        dashboard?.allPullRequests.contains { $0.url == selectedPullRequestURL } ?? false
    }
}

extension Dashboard {
    /// Review requested, then reviewed by me, then authored. A PR can be in several lists, so each appears once.
    var allPullRequests: [PullRequest] {
        var seen = Set<String>()
        return (reviewRequested + reviewedByMe + authored).filter { seen.insert($0.url).inserted }
    }
}

extension CoreError {
    /// The human-readable text from the Rust `Display` impl. The generated
    /// `localizedDescription` includes the Swift case name, so don't show that.
    var message: String {
        switch self {
        case .GhNotFound(let message), .Auth(let message), .Network(let message), .Api(let message),
             .Git(let message), .Agent(let message), .Storage(let message):
            message
        }
    }
}

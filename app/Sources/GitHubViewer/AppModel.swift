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
            dashboard = try await client.fetchDashboard()
            errorMessage = nil
            lastUpdated = Date()
        } catch let error as CoreError {
            errorMessage = error.message
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

extension CoreError {
    /// The human-readable text from the Rust `Display` impl. The generated
    /// `localizedDescription` includes the Swift case name, so don't show that.
    var message: String {
        switch self {
        case .GhNotFound(let message), .Auth(let message), .Network(let message), .Api(let message):
            message
        }
    }
}

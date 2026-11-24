import SwiftUI

/// The review window's sidebar: every open PR you're involved in, grouped by repo.
struct ReviewSidebar: View {
    @Bindable var model: AppModel
    @State private var collapsedRepos: Set<String> = []

    var body: some View {
        if let dashboard = model.dashboard {
            let groups = dashboard.repoGroups
            // A VStack rather than safe area insets, so rows never scroll under the banner or status.
            VStack(spacing: 0) {
                if !dashboard.warnings.isEmpty {
                    WarningBanner(warnings: dashboard.warnings)
                }
                List(selection: $model.selectedPullRequestURL) {
                    ForEach(groups) { group in
                        Section(isExpanded: isExpanded(group.repo)) {
                            ForEach(group.pullRequests, id: \.url) { pullRequest in
                                SidebarRow(pullRequest: pullRequest,
                                           isWaiting: group.waitingURLs.contains(pullRequest.url))
                            }
                        } header: {
                            RepoHeader(group: group)
                        }
                    }
                }
                .overlay {
                    if groups.isEmpty {
                        ContentUnavailableView("No Open Pull Requests", systemImage: "checkmark.circle",
                                               description: Text("Nothing waiting on you, and nothing of yours open."))
                    }
                }
                RefreshStatus(model: model)
                    .font(.caption)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 8)
            }
            // A PR picked in the menu bar may sit in a collapsed repo.
            .onChange(of: model.selectedPullRequest?.repo) { _, repo in
                if let repo { collapsedRepos.remove(repo) }
            }
        } else if let errorMessage = model.errorMessage {
            ErrorView(message: errorMessage, isRetrying: model.isRefreshing) {
                Task { await model.refresh() }
            }
            .frame(maxHeight: .infinity)
        } else {
            ProgressView("Loading pull requests…")
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private func isExpanded(_ repo: String) -> Binding<Bool> {
        Binding {
            !collapsedRepos.contains(repo)
        } set: { isExpanded in
            if isExpanded {
                collapsedRepos.remove(repo)
            } else {
                collapsedRepos.insert(repo)
            }
        }
    }
}

/// One collapsible sidebar section.
struct RepoGroup: Identifiable {
    let repo: String
    /// Newest first.
    let pullRequests: [PullRequest]
    /// The PRs in this repo that request your review.
    let waitingURLs: Set<String>

    var id: String { repo }
}

extension Dashboard {
    /// Repos with PRs waiting on your review first, then by their most recently updated PR.
    var repoGroups: [RepoGroup] {
        let waiting = Set(reviewRequested.map(\.url))
        return Dictionary(grouping: allPullRequests, by: \.repo)
            .map { repo, pullRequests in
                RepoGroup(repo: repo,
                          pullRequests: pullRequests.sorted { $0.updatedAt > $1.updatedAt },
                          waitingURLs: waiting.intersection(pullRequests.map(\.url)))
            }
            .sorted { a, b in
                if a.waitingURLs.isEmpty != b.waitingURLs.isEmpty {
                    return !a.waitingURLs.isEmpty
                }
                if a.pullRequests[0].updatedAt != b.pullRequests[0].updatedAt {
                    return a.pullRequests[0].updatedAt > b.pullRequests[0].updatedAt
                }
                // Dictionary order is random, so break ties to keep the list from jumping between refreshes.
                return a.repo < b.repo
            }
    }
}

private struct RepoHeader: View {
    let group: RepoGroup
    @Environment(\.palette) private var palette

    var body: some View {
        HStack(spacing: 6) {
            Text(group.repo)
                .foregroundStyle(palette.repo(group.repo))
                .lineLimit(1)
                .truncationMode(.middle)
            if !group.waitingURLs.isEmpty {
                Text("\(group.waitingURLs.count)")
                    .font(.caption2.weight(.semibold).monospacedDigit())
                    .foregroundStyle(.white)
                    .padding(.horizontal, 5)
                    .background(.tint, in: Capsule())
                    .help("\(group.waitingURLs.count) waiting on your review")
            }
        }
    }
}

private struct SidebarRow: View {
    let pullRequest: PullRequest
    let isWaiting: Bool

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            // Like Mail's unread dot: visible at a glance without competing with the status icons.
            Circle()
                .fill(.tint)
                .frame(width: 6, height: 6)
                .opacity(isWaiting ? 1 : 0)
                .help(isWaiting ? "Waiting on your review" : "")
            ReviewIcon(decision: pullRequest.reviewDecision)

            VStack(alignment: .leading, spacing: 2) {
                Text(pullRequest.title)
                    .lineLimit(2)
                Text(subtitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }

            Spacer(minLength: 4)

            if pullRequest.isDraft {
                DraftBadge()
            }
            CheckIcon(state: pullRequest.checkState)
        }
        .padding(.vertical, 2)
    }

    private var subtitle: String {
        var parts = ["#\(pullRequest.number)"]
        if let author = pullRequest.author {
            parts.append(author)
        }
        parts.append(relativeTime(pullRequest.updatedAt, now: Date()))
        return parts.joined(separator: " · ")
    }
}

import AppKit
import SwiftUI

/// The window shown when clicking the menu bar icon.
struct MenuView: View {
    static let maxListHeight: CGFloat = 480

    let model: AppModel
    @State private var listHeight: CGFloat = 0

    var body: some View {
        VStack(spacing: 0) {
            content
            Divider()
            FooterView(model: model)
        }
        .frame(width: 420)
        // The menu window is kept alive between openings, so onAppear may only
        // fire the first time; becoming key happens on every open.
        .onAppear {
            Task { await model.refreshIfStale() }
        }
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.didBecomeKeyNotification)) { _ in
            Task { await model.refreshIfStale() }
        }
    }

    @ViewBuilder
    private var content: some View {
        if let dashboard = model.dashboard {
            if !dashboard.warnings.isEmpty {
                WarningBanner(warnings: dashboard.warnings)
                Divider()
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    PRSection(title: "Review requested",
                              pullRequests: dashboard.reviewRequested,
                              emptyText: "Nothing waiting on your review")
                    PRSection(title: "My pull requests",
                              pullRequests: dashboard.authored,
                              emptyText: "You have no open pull requests")
                }
                .padding(.vertical, 6)
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { listHeight = $0 }
            }
            // Fit short lists exactly; scroll once they're taller than maxListHeight.
            .frame(height: min(listHeight, Self.maxListHeight))
        } else if let errorMessage = model.errorMessage {
            ErrorView(message: errorMessage, isRetrying: model.isRefreshing) {
                Task { await model.refresh() }
            }
        } else {
            ProgressView("Loading pull requests…")
                .frame(maxWidth: .infinity)
                .padding(40)
        }
    }
}

private struct PRSection: View {
    let title: String
    let pullRequests: [PullRequest]
    let emptyText: String

    var body: some View {
        HStack {
            Text(title)
            Text("\(pullRequests.count)")
                .foregroundStyle(.secondary)
        }
        .font(.headline)
        .padding(.horizontal, 16)
        .padding(.top, 10)
        .padding(.bottom, 4)

        if pullRequests.isEmpty {
            Text(emptyText)
                .foregroundStyle(.secondary)
                .padding(.horizontal, 16)
                .padding(.vertical, 6)
        } else {
            ForEach(pullRequests, id: \.url) { pullRequest in
                PRRow(pullRequest: pullRequest)
            }
        }
    }
}

private struct PRRow: View {
    let pullRequest: PullRequest
    @State private var isHovering = false

    var body: some View {
        Button {
            if let url = URL(string: pullRequest.url) {
                NSWorkspace.shared.open(url)
            }
        } label: {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
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
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .contentShape(Rectangle())
            .background(isHovering ? Color.primary.opacity(0.08) : .clear,
                        in: RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
        .padding(.horizontal, 6)
        .onHover { isHovering = $0 }
        .help("Open in browser")
    }

    private var subtitle: String {
        var parts = ["\(pullRequest.repo) #\(pullRequest.number)"]
        if let author = pullRequest.author {
            parts.append(author)
        }
        parts.append(relativeTime(pullRequest.updatedAt, now: Date()))
        return parts.joined(separator: " · ")
    }
}

private struct FooterView: View {
    let model: AppModel

    var body: some View {
        HStack(spacing: 8) {
            RefreshStatus(model: model)
            Spacer()
            RefreshButton(model: model)
                .buttonStyle(.borderless)

            Button("Quit") { NSApplication.shared.terminate(nil) }
                .buttonStyle(.borderless)
                .keyboardShortcut("q")
        }
        .font(.caption)
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}

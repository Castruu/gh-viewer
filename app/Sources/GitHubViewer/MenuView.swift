import AppKit
import SwiftUI

/// The window shown when clicking the menu bar icon.
struct MenuView: View {
    let model: AppModel

    var body: some View {
        VStack(spacing: 0) {
            content
            Divider()
            FooterView(model: model)
        }
        .frame(width: 420)
        .onAppear {
            Task { await model.refreshIfStale() }
        }
    }

    @ViewBuilder
    private var content: some View {
        if let dashboard = model.dashboard {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    PRSection(title: "Review requested",
                              pullRequests: dashboard.reviewRequested,
                              emptyText: "Nothing waiting on your review")
                    PRSection(title: "My pull requests",
                              pullRequests: dashboard.authored,
                              emptyText: "You have no open pull requests")
                }
                .padding(.vertical, 6)
            }
            .frame(height: 480)
        } else if let errorMessage = model.errorMessage {
            ErrorView(message: errorMessage) {
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
                    Text("Draft")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .overlay(Capsule().stroke(.secondary.opacity(0.6)))
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

private let relativeFormatter: RelativeDateTimeFormatter = {
    let formatter = RelativeDateTimeFormatter()
    formatter.unitsStyle = .abbreviated
    return formatter
}()

/// "3 hr. ago", or "just now" instead of the formatter's "in 0 sec.".
private func relativeTime(_ date: Date, now: Date) -> String {
    if now.timeIntervalSince(date) < 60 {
        return "just now"
    }
    return relativeFormatter.localizedString(for: date, relativeTo: now)
}

private struct ReviewIcon: View {
    let decision: ReviewDecision?

    var body: some View {
        Image(systemName: symbol)
            .foregroundStyle(color)
            .frame(width: 16)
            .help(label)
    }

    private var symbol: String {
        switch decision {
        case .approved: "checkmark.circle.fill"
        case .changesRequested: "xmark.circle.fill"
        case .reviewRequired: "clock"
        case nil: "circle.dashed"
        }
    }

    private var color: Color {
        switch decision {
        case .approved: .green
        case .changesRequested: .red
        case .reviewRequired: .orange
        case nil: .secondary
        }
    }

    private var label: String {
        switch decision {
        case .approved: "Approved"
        case .changesRequested: "Changes requested"
        case .reviewRequired: "Review required"
        case nil: "No review decision"
        }
    }
}

private struct CheckIcon: View {
    let state: CheckState?

    var body: some View {
        Circle()
            .fill(color)
            .frame(width: 8, height: 8)
            .opacity(state == nil ? 0 : 1)
            .help(label)
    }

    private var color: Color {
        switch state {
        case .success: .green
        case .failure: .red
        case .pending: .yellow
        case nil: .clear
        }
    }

    private var label: String {
        switch state {
        case .success: "Checks passed"
        case .failure: "Checks failed"
        case .pending: "Checks running"
        case nil: "No checks"
        }
    }
}

private struct ErrorView: View {
    let message: String
    let retry: () -> Void

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "exclamationmark.triangle")
                .font(.title)
                .foregroundStyle(.orange)
            Text(message)
                .multilineTextAlignment(.center)
                .textSelection(.enabled)
            Button("Try Again", action: retry)
        }
        .frame(maxWidth: .infinity)
        .padding(24)
    }
}

private struct FooterView: View {
    let model: AppModel

    var body: some View {
        HStack(spacing: 8) {
            status
            Spacer()
            Button {
                Task { await model.refresh() }
            } label: {
                if model.isRefreshing {
                    ProgressView().controlSize(.small)
                } else {
                    Image(systemName: "arrow.clockwise")
                }
            }
            .buttonStyle(.borderless)
            .disabled(model.isRefreshing)
            .keyboardShortcut("r")
            .help("Refresh (⌘R)")

            Button("Quit") { NSApplication.shared.terminate(nil) }
                .buttonStyle(.borderless)
                .keyboardShortcut("q")
        }
        .font(.caption)
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    @ViewBuilder
    private var status: some View {
        // Only show the error here when stale data is still on screen; otherwise ErrorView shows it.
        if let errorMessage = model.errorMessage, model.dashboard != nil {
            Label("Refresh failed", systemImage: "exclamationmark.triangle")
                .foregroundStyle(.orange)
                .help(errorMessage)
        } else if let lastUpdated = model.lastUpdated {
            TimelineView(.periodic(from: .now, by: 30)) { context in
                Text("Updated \(relativeTime(lastUpdated, now: context.date))")
                    .foregroundStyle(.secondary)
            }
        }
    }
}

import SwiftUI

// Views and helpers shared by the menu bar dropdown and the review window.

private let relativeFormatter: RelativeDateTimeFormatter = {
    let formatter = RelativeDateTimeFormatter()
    formatter.unitsStyle = .abbreviated
    return formatter
}()

/// "3 hr. ago", or "just now" instead of the formatter's "in 0 sec.".
func relativeTime(_ date: Date, now: Date) -> String {
    if now.timeIntervalSince(date) < 60 {
        return "just now"
    }
    return relativeFormatter.localizedString(for: date, relativeTo: now)
}

/// A PR's state as a colored dot and a word. One thing only, the one most worth knowing:
/// what's broken, then what's decided, then what's still running. Nothing for a PR with no news.
struct PRStatusText: View {
    let pullRequest: PullRequest
    @Environment(\.palette) private var palette

    var body: some View {
        if let status {
            HStack(spacing: 4) {
                Circle()
                    .fill(status.color)
                    .frame(width: 6, height: 6)
                Text(status.text)
                    .foregroundStyle(.secondary)
            }
            .lineLimit(1)
            .fixedSize()
        }
    }

    private var status: (text: String, color: Color)? {
        if pullRequest.checkState == .failure { return ("Checks failing", palette.fail) }
        if pullRequest.reviewDecision == .changesRequested { return ("Changes requested", palette.fail) }
        if pullRequest.isDraft { return ("Draft", .secondary) }
        if pullRequest.reviewDecision == .approved { return ("Approved", palette.pass) }
        if pullRequest.checkState == .pending { return ("Checks running", palette.pending) }
        return nil
    }
}

/// A PR in a list: what it does, then where it is, how old, and its state.
/// Who wrote it and the full title are in the tooltip and in the PR's header.
struct PRRowLabel: View {
    let pullRequest: PullRequest

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(pullRequest.title.withoutCommitPrefix)
                .lineLimit(1)
            HStack(spacing: 6) {
                Text(subtitle)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                Spacer(minLength: 4)
                PRStatusText(pullRequest: pullRequest)
            }
            .font(.caption)
        }
        .help("\(pullRequest.repo) #\(pullRequest.number) by \(pullRequest.author ?? "ghost"): \(pullRequest.title)")
    }

    /// The repo without its owner: the short name is what tells PRs apart.
    private var subtitle: String {
        "\(pullRequest.repo.split(separator: "/").last ?? "") #\(pullRequest.number) · \(relativeTime(pullRequest.updatedAt, now: Date()))"
            // The size tells a two-minute review from an afternoon.
            + " · +\(pullRequest.additions) −\(pullRequest.deletions)"
    }
}

extension String {
    /// "feat(skills): add a pr skill" as "Add a pr skill". A list reads faster when every
    /// row starts with what the PR does.
    var withoutCommitPrefix: String {
        guard let prefix = range(of: #"^[a-z]+(\([^)]*\))?!?: "#, options: .regularExpression),
              prefix.upperBound < endIndex
        else { return self }
        let rest = self[prefix.upperBound...]
        return rest.prefix(1).uppercased() + rest.dropFirst()
    }
}

struct ErrorView: View {
    @Environment(\.palette) private var palette
    let message: String
    let isRetrying: Bool
    let retry: () -> Void

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "exclamationmark.triangle")
                .font(.title)
                .foregroundStyle(palette.pending)
            Text(message)
                .multilineTextAlignment(.center)
                .textSelection(.enabled)
            if isRetrying {
                ProgressView().controlSize(.small)
            } else {
                Button("Try Again", action: retry)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(24)
    }
}

/// Shown when GitHub returned results with errors, e.g. an org requiring SAML SSO.
struct WarningBanner: View {
    @Environment(\.palette) private var palette
    let warnings: [String]

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(palette.pending)
            VStack(alignment: .leading, spacing: 2) {
                Text("Some pull requests may be missing")
                    .font(.callout.weight(.medium))
                Text(warnings.joined(separator: "\n"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(3)
                    .textSelection(.enabled)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 8)
        .help(warnings.joined(separator: "\n"))
    }
}

/// Refreshes the dashboard with ⌘R; shows a spinner while a refresh runs.
struct RefreshButton: View {
    let model: AppModel

    var body: some View {
        Button {
            Task { await model.refresh() }
        } label: {
            if model.isRefreshing {
                ProgressView().controlSize(.small)
            } else {
                Image(systemName: "arrow.clockwise")
            }
        }
        .disabled(model.isRefreshing)
        .keyboardShortcut("r")
        .help("Refresh (⌘R)")
    }
}

/// "Updated 2 min. ago", or a warning when the last refresh failed.
struct RefreshStatus: View {
    @Environment(\.palette) private var palette
    let model: AppModel

    var body: some View {
        // Only show the error here when stale data is still on screen; otherwise ErrorView shows it.
        if let errorMessage = model.errorMessage, model.dashboard != nil {
            Label("Refresh failed", systemImage: "exclamationmark.triangle")
                .foregroundStyle(palette.pending)
                .help(errorMessage)
        } else if let lastUpdated = model.lastUpdated {
            TimelineView(.periodic(from: .now, by: 30)) { context in
                Text("Updated \(relativeTime(lastUpdated, now: context.date))")
                    .foregroundStyle(.secondary)
            }
        }
    }
}

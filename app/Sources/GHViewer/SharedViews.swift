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

struct ReviewIcon: View {
    let decision: ReviewDecision?
    @Environment(\.palette) private var palette

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
        case .approved: palette.pass
        case .changesRequested: palette.fail
        case .reviewRequired: palette.pending
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

struct CheckIcon: View {
    let state: CheckState?
    @Environment(\.palette) private var palette

    var body: some View {
        Circle()
            .fill(color)
            .frame(width: 8, height: 8)
            .opacity(state == nil ? 0 : 1)
            .help(label)
    }

    private var color: Color {
        switch state {
        case .success: palette.pass
        case .failure: palette.fail
        case .pending: palette.pending
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

struct DraftBadge: View {
    var body: some View {
        Text("Draft")
            .font(.caption2)
            .foregroundStyle(.secondary)
            .padding(.horizontal, 5)
            .padding(.vertical, 1)
            .overlay(Capsule().stroke(.secondary.opacity(0.6)))
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

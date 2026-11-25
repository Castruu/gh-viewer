import SwiftUI

/// The selected PR's changed files and diff, with drafts, threads and review submission.
struct DiffPane: View {
    let review: PRReview
    /// The diff flattened for the lazy list; rebuilt only when the diff itself changes.
    @State private var files: [DiffFileModel] = []
    /// Collapse choices by path. Files without one follow their default: collapsed when viewed or large.
    @State private var expanded: [String: Bool] = [:]
    @State private var editor: DiffEditorTarget?
    @State private var scrollRequest: DiffScrollRequest?
    @State private var isSubmitPresented = false
    @State private var isConfirmingDiscard = false
    /// The last submitted review's URL, shown briefly as a confirmation.
    @State private var submittedURL: String?
    /// HSplitView can't set a starting width, so the split is drawn here; it's kept across PRs.
    @AppStorage("diffFileListWidth") private var fileListWidth = 260.0
    @Environment(\.palette) private var palette

    var body: some View {
        Group {
            if review.opened != nil {
                loaded
            } else if case .failed(let message) = review.loadState {
                ErrorView(message: message, isRetrying: false) { reload() }
                    .frame(maxHeight: .infinity)
            } else {
                VStack(spacing: 8) {
                    ProgressView()
                    Text("Checking out \(review.repo)…")
                    Text("The first time clones the repository, which can take a while.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .task {
            if review.loadState == .idle { await review.load() }
        }
        .onChange(of: review.diffKey, initial: true) { old, new in
            files = (review.opened?.files ?? []).enumerated().map { DiffFileModel($1, index: $0) }
            // Line numbers mean something else on a new diff.
            if old != nil && old != new {
                review.selection = nil
                editor = nil
            }
        }
        .sheet(isPresented: $isSubmitPresented) {
            SubmitReviewSheet(review: review) { submittedURL = $0 }
        }
    }

    private var loaded: some View {
        VStack(spacing: 0) {
            banners
            HStack(spacing: 0) {
                DiffFileList(review: review, files: files) { scrollRequest = DiffScrollRequest(path: $0) } setViewed: { setViewed($0, $1) }
                    .frame(width: fileListWidth)
                DiffSplitDivider(width: $fileListWidth, range: 180...480)
                VStack(spacing: 0) {
                    DiffSummaryBar(review: review, files: files, isSubmitPresented: $isSubmitPresented)
                    Divider()
                    if files.isEmpty {
                        ContentUnavailableView("No Changes", systemImage: "checkmark.circle",
                                               description: Text("This pull request doesn't change any files."))
                            .frame(maxHeight: .infinity)
                    } else {
                        DiffList(review: review, files: files, expanded: $expanded, editor: $editor,
                                 scrollRequest: scrollRequest) { setViewed($0, $1) }
                    }
                }
                .frame(minWidth: 360, maxWidth: .infinity)
                .overlay(alignment: .bottom) {
                    if let submittedURL {
                        SubmitConfirmation(url: submittedURL) { self.submittedURL = nil }
                            .task(id: submittedURL) {
                                try? await Task.sleep(for: .seconds(8))
                                self.submittedURL = nil
                            }
                    }
                }
            }
        }
    }

    @ViewBuilder
    private var banners: some View {
        if let error = review.actionError, !isSubmitPresented {
            DiffBanner(systemImage: "exclamationmark.triangle.fill", tint: palette.fail, message: error) {
                Button {
                    review.actionError = nil
                } label: {
                    Image(systemName: "xmark")
                }
                .buttonStyle(.borderless)
                .help("Dismiss")
            }
        }
        if case .failed(let message) = review.loadState {
            DiffBanner(systemImage: "exclamationmark.triangle.fill", tint: palette.pending,
                       message: "Couldn't reload, so this is the last diff that loaded. \(message)") {
                Button("Retry") { reload() }
                    .help("Fetch the PR and diff it again")
            }
        }
        if review.hasNewerCommits {
            let count = review.drafts.count
            DiffBanner(systemImage: "pin.fill", tint: palette.pending,
                       message: count == 0
                           ? "New commits were pushed. Reload to see the latest changes."
                           : "New commits were pushed. Your \(countLabel(count, "draft")) are on \(review.shortHeadOid). Submit or discard them to see the latest changes.") {
                if count == 0 {
                    Button("Reload") { reload() }
                        .help("Show the latest changes")
                } else {
                    Button("Discard Drafts…") { isConfirmingDiscard = true }
                        .help("Delete every draft on this PR and move to the latest changes")
                    Button("Submit Review…") { isSubmitPresented = true }
                        .help("Submit your drafts on \(review.shortHeadOid)")
                }
            }
            .confirmationDialog("Discard \(countLabel(count, "draft"))?", isPresented: $isConfirmingDiscard) {
                Button("Discard Drafts", role: .destructive) {
                    Task { await review.discardDrafts() }
                }
            } message: {
                Text("Your drafts and the agent's are deleted. They were never submitted, so they can't be recovered.")
            }
        }
    }

    private func reload() {
        Task { await review.load() }
    }

    /// Checking a file collapses it, as on GitHub, by dropping any collapse choice for it.
    private func setViewed(_ path: String, _ viewed: Bool) {
        expanded[path] = nil
        Task { await review.setViewed(path, viewed) }
    }
}

/// A drag handle that resizes the view to its left.
private struct DiffSplitDivider: View {
    @Binding var width: Double
    let range: ClosedRange<Double>
    @State private var startWidth: Double?

    var body: some View {
        Divider()
            .overlay {
                Color.clear
                    .frame(width: 9)
                    .contentShape(Rectangle())
                    .onHover { inside in
                        if inside { NSCursor.resizeLeftRight.push() } else { NSCursor.pop() }
                    }
                    .gesture(DragGesture(minimumDistance: 1, coordinateSpace: .global)
                        .onChanged { drag in
                            let start = startWidth ?? width
                            startWidth = start
                            width = min(max(start + drag.translation.width, range.lowerBound), range.upperBound)
                        }
                        .onEnded { _ in startWidth = nil })
            }
            .help("Drag to resize the file list")
    }
}

/// Above the diff: totals, Reload, and Submit Review with the draft count.
private struct DiffSummaryBar: View {
    let review: PRReview
    let files: [DiffFileModel]
    @Binding var isSubmitPresented: Bool

    var body: some View {
        let viewed = review.viewedPaths
        HStack(spacing: 12) {
            Text(countLabel(files.count, "file")).fontWeight(.medium)
            DiffLineCounts(additions: files.reduce(0) { $0 + $1.additions },
                           deletions: files.reduce(0) { $0 + $1.deletions })
            Text("\(files.filter { viewed.contains($0.id) }.count) of \(files.count) viewed")
                .foregroundStyle(.secondary)
            Spacer()
            Button {
                Task { await review.load() }
            } label: {
                if review.loadState == .loading {
                    ProgressView().controlSize(.small)
                } else {
                    Label("Reload", systemImage: "arrow.clockwise")
                }
            }
            .disabled(review.loadState == .loading)
            .help("Fetch the PR again to pick up new pushes and comments")
            Button {
                isSubmitPresented = true
            } label: {
                Text(review.drafts.isEmpty ? "Submit Review" : "Submit Review (\(review.drafts.count))")
            }
            .buttonStyle(.borderedProminent)
            .disabled(review.isSubmitting)
            .help("Submit your drafts as one review: comment, approve or request changes")
        }
        .font(.callout)
        .controlSize(.small)
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }
}

/// A full-width strip above the diff with an icon, a message and actions.
private struct DiffBanner<Actions: View>: View {
    let systemImage: String
    let tint: Color
    let message: String
    @ViewBuilder let actions: Actions

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: systemImage).foregroundStyle(tint)
            Text(message)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
            actions
        }
        .font(.callout)
        .controlSize(.small)
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(tint.opacity(0.12))
        .overlay(alignment: .bottom) { Divider() }
    }
}

/// "Review submitted", with a link to it on GitHub.
private struct SubmitConfirmation: View {
    let url: String
    let dismiss: () -> Void
    @Environment(\.palette) private var palette

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "checkmark.circle.fill").foregroundStyle(palette.pass)
            Text("Review submitted")
            if let link = URL(string: url) {
                Link("Open", destination: link)
                    .help("Open the review on GitHub")
            }
            Button(action: dismiss) {
                Image(systemName: "xmark")
            }
            .buttonStyle(.borderless)
            .help("Dismiss")
        }
        .font(.callout)
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .background(.regularMaterial, in: Capsule())
        .shadow(color: .black.opacity(0.2), radius: 6, y: 2)
        .padding(.bottom, 16)
    }
}

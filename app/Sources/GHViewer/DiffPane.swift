import SwiftUI

/// The selected PR's changed files and diff, with drafts, threads and review submission.
struct DiffPane: View {
    let review: PRReview
    /// Called with the review's URL once a submit went through.
    let submitted: (String) -> Void
    /// The diff flattened for the lazy list; rebuilt only when the diff itself changes.
    @State private var files: [DiffFileModel] = []
    /// Collapse choices by path. Files without one follow their default: collapsed when viewed or large.
    @State private var expanded: [String: Bool] = [:]
    @State private var editor: DiffEditorTarget?
    @State private var isSubmitPresented = false
    /// HSplitView can't set a starting width, so the split is drawn here; it's kept across PRs.
    @AppStorage("diffFileListWidth") private var fileListWidth = 230.0
    @AppStorage(DiffFileList.showsKey) private var showsFiles = true
    @State private var isConfirmingDiscard = false
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
            SubmitReviewSheet(review: review, submitted: submitted)
        }
        .focusedSceneValue(\.reviewActions, ReviewActions(
            nextFile: { goToFile(by: 1) },
            previousFile: { goToFile(by: -1) },
            toggleViewed: toggleViewedOnCurrentFile,
            comment: commentOnSelection,
            askAgent: { if review.selection != nil { review.chatFocusRequest = UUID() } },
            submit: { if review.opened != nil { isSubmitPresented = true } }))
    }

    private var shownFiles: [DiffFileModel] {
        review.shown(files, path: \.id)
    }

    private var loaded: some View {
        VStack(spacing: 0) {
            banners
            HStack(spacing: 0) {
                if showsFiles {
                    DiffFileList(review: review)
                        .frame(width: fileListWidth)
                    DiffSplitDivider(width: $fileListWidth, range: 180...480)
                }
                VStack(spacing: 0) {
                    DiffSummaryBar(review: review, files: shownFiles, isSubmitPresented: $isSubmitPresented)
                    Divider()
                    if shownFiles.isEmpty {
                        ContentUnavailableView("No Changes", systemImage: "checkmark.circle",
                                               description: Text("This pull request doesn't change any files."))
                            .frame(maxHeight: .infinity)
                    } else {
                        DiffList(review: review, files: shownFiles, expanded: $expanded, editor: $editor,
                                 scrollRequest: review.scrollRequest) { setViewed($0, $1) }
                    }
                }
                .frame(minWidth: 360, maxWidth: .infinity)
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
                    Label("Dismiss", systemImage: "xmark").labelStyle(.iconOnly)
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
                if let pending = review.unsettled?.reviewUrl {
                    Text("Your drafts and the agent's are deleted here. An earlier submit put some in your pending review on github.com, which isn't deleted and may still be there: submit or delete it there (\(pending)).")
                } else {
                    Text("Your drafts and the agent's are deleted here. Ones that haven't been posted can't be recovered.")
                }
            }
        }
    }

    private func reload() {
        Task { await review.load() }
    }

    /// Moves the diff to the file after or before the current one: the one the tree or this last went to.
    private func goToFile(by step: Int) {
        let paths = shownFiles.map(\.id)
        if let path = Self.path(after: review.scrollRequest?.path, by: step, in: paths) {
            review.scrollRequest = DiffScrollRequest(path: path)
        }
    }

    /// The first file when there's no current one; `nil` past either end.
    static func path(after current: String?, by step: Int, in paths: [String]) -> String? {
        guard let index = paths.firstIndex(where: { $0 == current }) else { return paths.first }
        return paths.indices.contains(index + step) ? paths[index + step] : nil
    }

    /// Marks the current file, whose header is tinted, and moves on to the next one.
    private func toggleViewedOnCurrentFile() {
        guard let path = review.scrollRequest?.path, review.canMarkViewed else { return }
        let wasViewed = review.isViewed(path)
        setViewed(path, !wasViewed)
        if !wasViewed { goToFile(by: 1) }
    }

    /// Opens the comment editor under the selected lines, where the Comment button would.
    private func commentOnSelection() {
        guard editor == nil, !review.isReloading, let selection = review.selection,
              let file = files.first(where: { $0.id == selection.path }),
              let rows = file.rowRange(of: selection), !file.crossesHunks(rows)
        else { return }
        editor = .newComment(selection)
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
            if let changed = review.opened?.changedSinceReview {
                if changed.isEmpty {
                    Text("Nothing new since your review")
                        .foregroundStyle(.secondary)
                } else {
                    Toggle("Since Your Review", isOn: Binding { review.showsOnlyChangedSinceReview } set: { review.showsOnlyChangedSinceReview = $0 })
                        .toggleStyle(.button)
                        .help("Show only the files that changed since the last review you submitted")
                }
            }
            Spacer()
            Button {
                Task { await review.load() }
            } label: {
                if review.loadState == .loading {
                    ProgressView().controlSize(.small)
                } else {
                    Label("Reload", systemImage: "arrow.clockwise")
                        .labelStyle(.iconOnly)
                }
            }
            .buttonStyle(.borderless)
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
        .padding(.horizontal, 20)
        .padding(.vertical, 8)
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
struct SubmitConfirmation: View {
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
                Label("Dismiss", systemImage: "xmark").labelStyle(.iconOnly)
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
